//! The Background Task Ledger (ADR-0060).
//!
//! The card-tail section that lists a Session's live Background Tasks — task
//! type, bolded label, start clock, a shell row's elapsed, and a background
//! subagent's child activity (spec #501) — riding the newest card of its Card
//! Chain like the Todo Panel and the live Tool Panels. One folded collapsible
//! panel: the pinned count (`⏳ 后台任务（N）`) is its title, so it stays readable
//! folded, and the rows are its body. The Bridge gathers the facts (which tasks
//! are live, what label their originating tool part's input names, what the
//! task's child is doing when it has one) and hands them over as
//! [`TaskLedgerRow`]s; this module owns the pinned copy, the formats and
//! nothing else.
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

use super::sanitize::escaped_entities;
use super::tool_render::TaskLiveness;
use super::{first_n_chars_bytes, fmt_local_time, truncate_md};
use crate::backend::ShellOutputWindow;

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

/// What a shell completion entry whose record cannot be read says in place of
/// its tail (spec #588, #593): the record was gone (server restart, the
/// 25-record ceiling, retention) or the read failed — never an empty panel
/// posing as output.
const OUTPUT_UNAVAILABLE: &str = "输出已不可用";

/// The cleanup action's label (spec #588, ticket #590): the ONE section-level
/// button a Waiting card carrying unconfirmed rows offers below the ledger —
/// the user's exit for a wait no machine can confirm. Pinned here with the
/// ledger's other copy.
pub(crate) const CLEANUP_BUTTON_TEXT: &str = "清理待确认任务";

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
            TaskEnding::Cleaned => "已清理",
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
    /// The user's own cleanup click retired the task (spec #588, ticket #590):
    /// no machine could confirm it, so the waiting card's 清理待确认任务 button
    /// ended it. Never derived by a read — the Bridge synthesizes this ending
    /// from the click, together with the retirement overlay record.
    Cleaned,
}

impl TaskEnding {
    /// Whether this ending was the user's own cleanup (spec #588, #590): the
    /// title swaps the completion bell for the broom and appends the 人工
    /// marker, so a manual retirement never reads like the runtime's or a
    /// Wake's.
    fn is_manual(&self) -> bool {
        matches!(self, Self::Cleaned)
    }
}

/// One live Background Task as its ledger row renders it: the task's type, the
/// label joined from the originating tool part's input by `call_id` (`None`
/// when that input names no label — the row then renders bare), when the run
/// started (`None` renders no start clock; a shell row also renders its elapsed
/// from it, a subagent row never does), whether a runtime reconciliation could
/// not confirm it as running (issue #454 — the row then carries the
/// 状态待确认 marker, and only its own Wake or a positive terminal verdict can
/// retire it), and the task's child-session liveness when it has one (spec
/// #501 — a background `subagent` row carries the same activity fragment the
/// live `task` panel shows, while a `shell` row has none).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLedgerRow {
    pub kind: TaskKind,
    pub label: Option<String>,
    pub started_at: Option<i64>,
    /// True when a reconciliation read reported the task not running while no
    /// Wake retired it. The row stays live (the runtime read is evidence, not
    /// an ending) but reads as unconfirmed.
    pub unconfirmed: bool,
    /// The child session's last successfully gathered liveness (spec #501),
    /// rendered as the front task panel's own fragment — `bash 5s`,
    /// `思考中 1m30s · 等待你的授权` — after the row's start clock. `None` for a
    /// shell row, before the child has been read, and while a read cannot
    /// establish one; the timestamps inside are never refreshed by a failed
    /// read, so the rendered age keeps growing truthfully (ADR-0054).
    pub activity: Option<TaskLiveness>,
    /// The shell's captured output window (spec #588, ticket #592), rendered
    /// as a labelled code block under the row: the last lines the shared
    /// reconcile read established, or `None` when the read spent nothing, the
    /// record had nothing to show, or the read failed — never a placeholder.
    /// Only a `shell` row ever carries one.
    pub output: Option<ShellOutputWindow>,
}

impl TaskLedgerRow {
    /// The activity fragment this row RENDERS: `None` while the row is
    /// unconfirmed — an unverified liveness must not read as certain (issue
    /// #454) — and `None` before any gather established one. The one predicate
    /// the body, the render clock and the size estimate share, so the three
    /// cannot drift over when a fragment is shown.
    fn rendered_activity(&self) -> Option<&TaskLiveness> {
        self.activity.as_ref().filter(|_| !self.unconfirmed)
    }

    /// The label as the row renders it: the same folded/clipped bold shape
    /// [`task_ledger_text`] emits, or `None` when the renderer omits it (no
    /// label, or an empty one — the renderer's own `!is_empty` filter).
    fn rendered_label(&self) -> Option<String> {
        self.label
            .as_deref()
            .filter(|label| !label.is_empty())
            .map(bold_label)
    }

    /// The start clock as the row renders it — local `HH:MM`, or `None` when
    /// the start is absent (or out of range). The elapsed is the render clock's
    /// business ([`task_ledger_clock`]), and only a shell row renders one.
    fn rendered_clock(&self) -> Option<String> {
        self.started_at.and_then(fmt_local_time)
    }

    /// The output facts the row RENDERS (spec #592): the window's text and
    /// whether it is clipped. The cutoff's `HH:MM` is the render clock's
    /// business ([`task_ledger_clock`]) — two captures inside the minute the
    /// label shows are one rendered window and owe no PATCH.
    fn rendered_output(&self) -> Option<(&str, bool)> {
        self.output
            .as_ref()
            .filter(|window| !window.text.is_empty())
            .map(|window| (window.text.as_str(), window.clipped))
    }

    /// Whether this row RENDERS like `previous` (ADR-0060's flush rule, spec
    /// #501): the visible facts only — the type, the label as the row folds,
    /// clips and bolds it (an empty label as absent), the local `HH:MM` start
    /// clock, the unconfirmed flag, the output window's text and clipping
    /// (spec #592, its cutoff minute is the clock's), and, while the row is
    /// confirmed, the fragment's label and wait — never the raw label/start
    /// values that render identically, nor the gathered liveness's stored
    /// timestamps. The rendered numbers (a shell row's elapsed, the fragment's
    /// age and the window's cutoff) are the render clock's business
    /// ([`task_ledger_clock`]), compared at the path's own cadence: text past
    /// the clip, an empty label, a start moving inside the displayed minute
    /// and elapsed second, or a child part landing inside the shown second must
    /// not owe a PATCH.
    pub(crate) fn renders_like(&self, previous: &Self) -> bool {
        self.kind == previous.kind
            && self.rendered_label() == previous.rendered_label()
            && self.rendered_clock() == previous.rendered_clock()
            && self.unconfirmed == previous.unconfirmed
            && self.rendered_output() == previous.rendered_output()
            && (self.unconfirmed
                || match (&self.activity, &previous.activity) {
                    (None, None) => true,
                    (Some(mine), Some(theirs)) => {
                        let (mine, theirs) = (mine.title_parts(), theirs.title_parts());
                        // Whether a fragment shows an age is its shape too, but
                        // the clock comparison already catches a `None`/`Some`
                        // age crossing (its rendered seconds go from nothing to
                        // a number and back), so the shape here is the words.
                        mine.label == theirs.label && mine.wait == theirs.wait
                    }
                    _ => false,
                })
    }
}

/// One shell completion entry's output fact (spec #588, ticket #593): the
/// result its fold body carries under the identity line. The Bridge reads it
/// once, when the entry renders — [`TaskOutput::Unavailable`] when the record
/// was gone or the read failed, so the body says 「输出已不可用」 rather than
/// posing an empty panel as output; a successful empty capture carries no fact
/// at all, so the entry stays identity-only (spec #588, review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutput {
    /// The record answered with the tail to show: the same bounded window the
    /// live row renders ([`ShellOutputWindow`]), labelled 截至于 HH:MM and
    /// clipped when the capture held more. Never an empty window — the
    /// readable-empty capture leaves the entry's output `None`.
    Window(ShellOutputWindow),
    /// The read was spent and there is nothing to show — a failed or vanished
    /// record. The body says 「输出已不可用」.
    Unavailable,
}

/// One completed Background Task as its ledger entry renders it (ADR-0060):
/// the mechanical completion line as the collapsed title, the task's identity
/// and its run's own server-time span in the fold — plus, for a shell the read
/// could answer for, the output tail (spec #588, #593). The Bridge gathers the
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
    /// The shell's output tail as the fold body renders it (spec #588, #593):
    /// the last bounded lines with their 截至于/已截断 labels after the
    /// identity line, or [`TaskOutput::Unavailable`]'s 「输出已不可用」 when the
    /// record could not be read. `None` when the ending shows no output (a
    /// subagent, the 已失联 ending, the 🧹 cleanup) or when no read was spent,
    /// so the body stays identity-only.
    pub output: Option<TaskOutput>,
}

impl TaskCompletionEntry {
    /// Whether this entry's ending shows the shell's output (spec #588,
    /// #593): only a shell's Wake ending (完成/取消/失败) or its runtime
    /// 结束. The 已失联 entry has no record to show by definition, a 🧹
    /// cleanup is the user's own ending, and a subagent has no shell output —
    /// all stay identity-only whatever `output` holds.
    pub(crate) fn shows_output(&self) -> bool {
        self.kind == TaskKind::Shell
            && matches!(self.ending, TaskEnding::Wake { .. } | TaskEnding::RuntimeEnded)
    }
}

/// The completion entry's collapsed title (ADR-0060) — the mechanical
/// completion line the merged-path receipt always carried,
/// `🔔 shell 完成：<label>` / `🔔 subagent 完成：<label>`, bare when the Wake
/// named no label. A manual cleanup (spec #588, #590) swaps the bell for the
/// broom and marks the ending 人工: `🧹 shell 已清理：<label>（人工）`. The label
/// goes through the shared [`folded_label`], so one label has one visible shape
/// wherever the ledger renders it.
pub(crate) fn task_entry_title(entry: &TaskCompletionEntry) -> String {
    let noun = entry.kind.completion_noun(&entry.ending);
    let (icon, manual) = if entry.ending.is_manual() {
        ("🧹", "（人工）")
    } else {
        ("🔔", "")
    };
    match entry.label.as_deref().filter(|label| !label.is_empty()) {
        Some(label) => format!("{icon} {noun}：{}{manual}", folded_label(label)),
        None => format!("{icon} {noun}{manual}"),
    }
}

/// The cleanup button a Waiting card carrying unconfirmed rows renders below
/// the ledger (spec #588, ticket #590): one section-level action whose click
/// clears every ⚠️ 状态待确认 row. The value carries the session id alone, like
/// the recovery buttons — the handler resolves the Chat/Topic from the
/// SessionStore.
pub(crate) fn cleanup_button(session_id: &str) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": CLEANUP_BUTTON_TEXT },
        "type": "default",
        "value": { "action": "cleanup", "session_id": session_id },
    })
}

/// The completion entry's fold body: the task's identity and the run's timing —
/// `shell sh_abc · 14:02 · 12m`, and `subagent ses_child · 14:02 · 1m` for the
/// subagent kind, whose noun is the ledger's own ([`TaskKind::noun`]) — plus,
/// under it, the shell's output tail (spec #588, #593) when the ending shows
/// one. The timing parts carry no Chinese labels (ADR-0060). Each part is
/// omitted when the read named none (an id-less or start-less entry stays
/// honest rather than inventing detail), the duration is the run's own
/// server-time span (finished − started), so re-rendering the entry never
/// drifts, and a task with no completion time (the lost ending) renders
/// identity only.
pub(crate) fn task_entry_body(entry: &TaskCompletionEntry) -> String {
    let mut body = entry.kind.noun().to_string();
    if let Some(id) = entry.id.as_deref().filter(|id| !id.is_empty()) {
        body.push(' ');
        body.push_str(id);
    }
    match entry.finished_at {
        Some(finished) => {
            if let Some(clock) = fmt_local_time(finished) {
                body.push_str(&format!(" · {clock}"));
            }
            if let Some(started) = entry.started_at {
                body.push_str(&format!(
                    " · {}",
                    fmt_entry_elapsed(secs_since(started, finished))
                ));
            }
        }
        // A task with no completion time (the lost ending) renders identity
        // only — never an invented clock, duration or output line.
        None => return body,
    }
    // The shell's own result follows the identity line (spec #588, #593): the
    // same labelled, fenced window the live row renders, or the honest
    // 「输出已不可用」 when the record could not be read. Only the endings that
    // show an output carry one ([`TaskCompletionEntry::shows_output`]).
    if entry.shows_output() {
        match &entry.output {
            Some(TaskOutput::Window(window)) if !window.text.is_empty() => {
                body.push('\n');
                body.push_str(&output_window_block(window));
            }
            Some(TaskOutput::Unavailable) => {
                body.push_str("\n  ");
                body.push_str(OUTPUT_UNAVAILABLE);
            }
            _ => {}
        }
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
/// · subagent：**review the diff** · 14:04 · bash 5s
/// ```
///
/// `None` when no task is live (the title's own emptiness rule, kept here so
/// the two renderings cannot drift). The type word stays plain, the label is
/// bolded through [`bold_label`], then the task's server `started_at` renders
/// as its local `HH:MM` start clock (the completion entry body's own clock).
/// A `shell` row's bare elapsed follows it — the elapsed is a shell row's only
/// liveness — while a `subagent` row never renders a total: its liveness is its
/// child's activity fragment ([`TaskLiveness::title_fragment`]), appended after
/// the clock (`bash 5s`, `思考中 1m30s · 等待你的授权`), so the fragment's age is
/// the only runtime it shows. A part the read named none of is omitted whole:
/// `· shell · 14:02 · 0m05s`, `· shell：**npm run build**`, `· shell`,
/// `· subagent：**review the diff** · 14:04`. A row a runtime reconciliation
/// could not confirm as running carries the trailing 状态待确认 marker (issue
/// #454), after every part it did carry — and renders no activity, whatever
/// fragment is still stored: an unverified liveness must not read as certain.
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
            // A shell row's elapsed is its only liveness, so it renders it; a
            // subagent row's liveness is its child's activity fragment, and its
            // total runtime would only restate the fragment's age — never
            // rendered.
            if row.kind == TaskKind::Shell {
                text.push_str(&format!(" · {}", fmt_task_elapsed(secs_since(at, now_ms))));
            }
        }
        if let Some(activity) = row.rendered_activity() {
            // A tool name is transcript text reaching a markdown body: neuter
            // its own `&`/`*`/`_` exactly like a label's, so it cannot bleed
            // formatting into the row either.
            text.push_str(&format!(
                " · {}",
                escaped_entities(&activity.title_fragment(now_ms))
            ));
        }
        if row.unconfirmed {
            text.push_str(UNCONFIRMED_MARKER);
        }
        // The shell's output window (spec #592) reads UNDER its row: the
        // labelled, fenced last lines. A window with no text renders nothing
        // (never an empty panel posing as output).
        if let Some(window) = row.output.as_ref().filter(|window| !window.text.is_empty()) {
            text.push('\n');
            text.push_str(&output_window_block(window));
        }
    }
    Some(text)
}

/// The output window's own label (spec #588, #592): the pinned copy
/// `截至于 HH:MM`, with ` · 仅最后 N 行 · 已截断` appended when the record held
/// more than the window shows. N is the lines the window RENDERS (the retained
/// count), so a byte-clipped window whose lines were huge names its real
/// count. An unformattable clock (out of range) drops its part rather than
/// inventing one.
fn output_window_label(window: &ShellOutputWindow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(clock) = fmt_local_time(window.captured_ms) {
        parts.push(format!("截至于 {clock}"));
    }
    if window.clipped {
        parts.push(format!("仅最后 {} 行 · 已截断", window.text.lines().count()));
    }
    parts.join(" · ")
}

/// One shell's output window as the ledger body renders it (spec #588, #592):
/// the indented label line, then the tail as a fenced code block so the
/// command's own markdown cannot bleed into the section. The fence is one
/// backtick longer than the longest run inside the tail (at least three), so a
/// tail line can never close the block early; the sanitizer passes fenced
/// content through verbatim.
fn output_window_block(window: &ShellOutputWindow) -> String {
    let mut block = String::new();
    let label = output_window_label(window);
    if !label.is_empty() {
        block.push_str("  ");
        block.push_str(&label);
        block.push('\n');
    }
    block.push_str(&output_fence(&window.text));
    block.push('\n');
    block.push_str(&window.text);
    block.push('\n');
    block.push_str(&output_fence(&window.text));
    block
}

/// The fence one output window needs: one backtick past the longest backtick
/// run in the tail, never under the markdown minimum of three. A run computed
/// per line, so a multi-line tail cannot smuggle a closer past it.
fn output_fence(text: &str) -> String {
    let mut longest = 0;
    for line in text.lines() {
        let mut run = 0;
        for c in line.chars() {
            if c == '`' {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// One row's rendered clock (ADR-0060, spec #501): every rendered number that
/// time moves — a shell row's elapsed, and any row's activity fragment age when
/// it shows one — in whole seconds. The Bridge compares it at the cadence each
/// path owes (the Turn's `LedgerCadence`): a row whose rendered seconds did not
/// move owes nothing. `None` where the row renders no such number (a subagent
/// row never renders an elapsed; no start time; no activity; an unconfirmed
/// row, which renders no activity whatever it stores; an untimed tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LedgerRowClock {
    pub elapsed: Option<u64>,
    pub activity: Option<u64>,
    /// The output window's rendered cutoff (`截至于 HH:MM`, spec #592) in whole
    /// seconds, minute-aligned so the label's own granularity is what moves.
    /// `None` where the row renders no window.
    pub output: Option<u64>,
}

/// The ledger's render clock (ADR-0060): a shell row's elapsed — and any row's
/// activity fragment age — in whole seconds against `now_ms`, in row order, so
/// the clock records what the card last rendered. The Bridge compares it at the
/// cadence each path owes (`LedgerCadence`): the live render at whole minutes
/// (its flushes are content-driven; a per-render second clock would be churn),
/// the yielded refresh at whole seconds (its ~8 s reads are a waiting card's
/// only clock). A part the row does not render has no clock (`None`) — a
/// subagent row's total runtime included — and a start skewed into the future
/// clamps like the render it keys.
pub(crate) fn task_ledger_clock(rows: &[TaskLedgerRow], now_ms: i64) -> Vec<LedgerRowClock> {
    rows.iter()
        .map(|row| LedgerRowClock {
            // Only what the row renders ticks: the elapsed is the shell row's
            // alone, so a subagent's start moving can never owe a flush.
            elapsed: match row.kind {
                TaskKind::Shell => row.started_at.map(|at| secs_since(at, now_ms)),
                TaskKind::Subagent => None,
            },
            activity: row
                .rendered_activity()
                .and_then(TaskLiveness::age_clock_ms)
                .map(|at| secs_since(at, now_ms)),
            output: row.output.as_ref().map(window_clock_secs),
        })
        .collect()
}

/// The output window's rendered cutoff in whole seconds, minute-aligned (spec
/// #592): the label renders `HH:MM` (local time, offsets are whole minutes), so
/// the live path's minute cadence and the yielded path's second cadence both
/// see exactly the label's own change — a re-read inside the shown minute owes
/// nothing, a crossing minute owes its flush.
fn window_clock_secs(window: &ShellOutputWindow) -> u64 {
    (window.captured_ms.max(0) as u64) / 60_000 * 60
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
    format!("**{}**", escaped_entities(&folded_label(label)))
}

/// Estimated serialized size (bytes) of the ledger section, for the card
/// splitter's tail reserve: the folded panel's title and element overhead, plus
/// one row per task — its ` · HH:MM` start clock, a shell row's ` · XmYYs`
/// elapsed, its bolded label, counted exactly as [`bold_label`] renders it
/// (clipped at [`TASK_LABEL_CHARS`], entity-escaped and wrapped), and its
/// activity fragment when it renders one ([`activity_estimate`]) — so the
/// estimate and the row cannot drift apart. The per-row constant stays the
/// shell-sized one: a subagent row's missing elapsed is over-reserve, which is
/// safe. Rough like the Bridge's `panel_estimate` for the tail's other
/// sections.
pub(crate) fn task_ledger_estimate(rows: &[TaskLedgerRow]) -> usize {
    let labels: usize = rows
        .iter()
        .filter_map(|row| row.label.as_deref().filter(|label| !label.is_empty()))
        .map(|label| bold_label(label).len())
        .sum();
    let activities: usize = rows
        .iter()
        .filter_map(|row| row.rendered_activity().map(activity_estimate))
        .sum();
    // Each output window (spec #592) costs exactly what its block renders —
    // the label, the fence and the tail — measured from the same builder the
    // body uses, plus the newline the row inserts before it.
    let outputs: usize = rows
        .iter()
        .filter_map(|row| row.output.as_ref().filter(|window| !window.text.is_empty()))
        .map(|window| output_window_block(window).len() + 1)
        .sum();
    // The +80 is the folded panel's own element overhead, exactly like the
    // completion entry's estimate charges it (`task_entry_estimate`).
    300 + labels + activities + outputs + rows.len() * 120 + 80
}

/// Estimated bytes of one row's activity fragment, measured from the same
/// parts [`TaskLiveness::title_fragment`] renders (via
/// [`TaskLiveness::title_parts`]) so the reserve cannot undercount what the
/// row shows:
///
/// ```text
/// <escaped label>[ <age>][ · <wait>]
/// ```
///
/// The label's cost is [`escaped_entities`]'s exactly (the row escapes the
/// fragment's tool name for the markdown body, and `*`/`_`/`&` expand three-
/// to fivefold), the age reserves the widest `fmt_elapsed` shape under a day
/// (`59m59s`), and the wait is its own words behind the same ` · ` separator
/// the renderer joins with. The age's exact width varies with the clock; the
/// reserve takes the widest, like the rest of the card's sizing.
fn activity_estimate(activity: &TaskLiveness) -> usize {
    /// The widest second-granular age [`fmt_elapsed`](super::shell::fmt_elapsed)
    /// renders below a day: `59m59s`.
    const AGE_MAX_CHARS: usize = 6;
    let parts = activity.title_parts();
    let label = escaped_entities(parts.label).len();
    let age = parts.age_clock_ms.map_or(0, |_| 1 + AGE_MAX_CHARS);
    let wait = parts.wait.map_or(0, |wait| " · ".len() + wait.len());
    label + age + wait
}

/// Estimated serialized size (bytes) of one completion entry's folded panel,
/// for the card splitter's timeline accounting: the title (its label clipped
/// like [`task_entry_title`] clips it), the body's identity, clock and output
/// tail (spec #593), and the panel's element overhead. Owned here so it cannot
/// drift from the render.
pub(crate) fn task_entry_estimate(entry: &TaskCompletionEntry) -> usize {
    let label = entry
        .label
        .as_deref()
        .map(|label| first_n_chars_bytes(label, TASK_LABEL_CHARS))
        .unwrap_or(0);
    let id = entry.id.as_deref().map(str::len).unwrap_or(0);
    300 + label + id + entry_output_estimate(entry) + 80
}

/// The bytes one completion entry's output adds to the body (spec #588,
/// #593): the same block builder the body renders, plus the newline that
/// separates it from the identity line, or the 输出已不可用 line. Zero for
/// every ending that shows no output — measured from the render, so the
/// estimate and the body cannot drift.
fn entry_output_estimate(entry: &TaskCompletionEntry) -> usize {
    if !entry.shows_output() {
        return 0;
    }
    match &entry.output {
        Some(TaskOutput::Window(window)) if !window.text.is_empty() => output_window_block(window).len() + 1,
        Some(TaskOutput::Unavailable) => "\n  ".len() + OUTPUT_UNAVAILABLE.len(),
        _ => 0,
    }
}

/// A shell ledger row's elapsed: bare, with no Chinese label (ADR-0060) —
/// `3m12s`, `1m05s` under an hour, `1h05m` above it. A subagent row never
/// renders one (its liveness is the activity fragment). Seconds are zero-padded
/// so the rows stay visually aligned; the minutes above an hour are too.
fn fmt_task_elapsed(secs: u64) -> String {
    if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The completion entry's duration: minute-granular, so a fixed entry reads
/// the same on every re-render and matches the pinned body (`12m`). Bare like a
/// shell row's elapsed, with the same `XhYYm` shape above an hour.
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
    use crate::feishu::card::tool_render::ChildActivity;

    /// A shell row's output window (spec #588, #592) renders under the row as
    /// the pinned copy: the cutoff label (`截至于 HH:MM`) then the tail as a
    /// fenced code block, so the command's own lines cannot break the section.
    #[test]
    fn a_shell_rows_output_window_renders_under_the_row() {
        let start = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let cut = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 5);
        let now = start + 192_000; // 14:05:12, elapsed 3m12s
        let row = |window: Option<ShellOutputWindow>| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some("gh run watch".into()),
            started_at: Some(start),
            unconfirmed: false,
            activity: None,
            output: window,
        };
        let window = |text: &str, clipped: bool| ShellOutputWindow {
            text: text.into(),
            clipped,
            captured_ms: cut,
        };

        assert_eq!(
            task_ledger_text(&[row(Some(window("line 1\nline 2", false)))], now).unwrap(),
            "· shell：**gh run watch** · 14:02 · 3m12s\n  截至于 14:05\n```\nline 1\nline 2\n```",
            "the window sits under its row, labelled and fenced"
        );
        // A row with no window renders its old shape; an empty window (never
        // produced by the read, but possible in a script) renders nothing too:
        // never an empty panel posing as output.
        assert_eq!(
            task_ledger_text(&[row(None)], now).unwrap(),
            "· shell：**gh run watch** · 14:02 · 3m12s"
        );
        assert_eq!(
            task_ledger_text(&[row(Some(window("", false)))], now).unwrap(),
            "· shell：**gh run watch** · 14:02 · 3m12s"
        );
    }

    /// A clipped window names its own truncation and its line count: the pinned
    /// 「仅最后 N 行 · 已截断」 appends to the cutoff, N being what the window
    /// actually renders.
    #[test]
    fn a_clipped_window_names_its_line_count() {
        let cut = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 5);
        let row = |text: String| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: None,
            started_at: None,
            unconfirmed: false,
            activity: None,
            output: Some(ShellOutputWindow {
                text,
                clipped: true,
                captured_ms: cut,
            }),
        };
        let fifteen = (1..=15)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = task_ledger_text(&[row(fifteen)], 0).unwrap();
        assert!(
            rendered.starts_with("· shell\n  截至于 14:05 · 仅最后 15 行 · 已截断\n```\nline 1"),
            "the clipped copy names the lines and the cutoff: {rendered}"
        );
        let rendered = task_ledger_text(&[row("a\nb\nc".into())], 0).unwrap();
        assert!(
            rendered.contains("截至于 14:05 · 仅最后 3 行 · 已截断"),
            "N is the lines the window renders: {rendered}"
        );
    }

    /// A tail carrying fence characters cannot close the window's code block:
    /// the fence grows one backtick past the longest run, and the sanitizer
    /// passes the fenced content through verbatim (spec #592).
    #[test]
    fn a_windows_fence_outgrows_the_tail() {
        use crate::feishu::card::CardState;
        use crate::feishu::card::shell::CardBuilder;

        let cut = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 5);
        let row = TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some("npm run build".into()),
            started_at: None,
            unconfirmed: false,
            activity: None,
            output: Some(ShellOutputWindow {
                text: "before\n```\n<number_tag> 的 ```\nafter".into(),
                clipped: true,
                captured_ms: cut,
            }),
        };
        let rendered = task_ledger_text(std::slice::from_ref(&row), 0).unwrap();
        let expected = "· shell：**npm run build**\n  截至于 14:05 · 仅最后 4 行 · 已截断\n\
                        ````\nbefore\n```\n<number_tag> 的 ```\nafter\n````";
        assert_eq!(rendered, expected, "the fence outgrows the tail's longest run");

        // Through the builder's sanitizer the fenced content is verbatim: the
        // `<` stays authored and the section cannot break out of the block.
        let built = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_task_ledger(std::slice::from_ref(&row))
            .build();
        let body = built["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(body, expected, "one clean body, fence intact: {built}");
    }

    /// The window's cutoff is a rendered clock, not a row fact (spec #592):
    /// re-reads inside the rendered minute don't owe a PATCH, a crossing minute
    /// does, and the clock carries the label's own minute — while different
    /// tail text or clipping is a row change.
    #[test]
    fn the_windows_cutoff_is_the_render_clocks_business() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 5);
        let row = |text: &str, clipped: bool, captured_ms: i64| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: None,
            started_at: None,
            unconfirmed: false,
            activity: None,
            output: Some(ShellOutputWindow {
                text: text.into(),
                clipped,
                captured_ms,
            }),
        };
        let bare = TaskLedgerRow {
            kind: TaskKind::Shell,
            label: None,
            started_at: None,
            unconfirmed: false,
            activity: None,
            output: None,
        };

        assert!(
            row("a", false, at).renders_like(&row("a", false, at + 30_000)),
            "a re-read inside the shown minute is one rendered window"
        );
        assert!(
            row("a", false, at).renders_like(&row("a", false, at + 60_000)),
            "the cutoff minute is the clock's half, not the row's"
        );
        assert!(!row("a", false, at).renders_like(&row("b", false, at)));
        assert!(!row("a", false, at).renders_like(&row("a", true, at)));
        assert!(!row("a", false, at).renders_like(&bare), "appearing is a change");
        assert!(!bare.renders_like(&row("a", false, at)), "leaving too");

        let clock = task_ledger_clock(&[row("a", false, at)], 0);
        assert_eq!(
            clock[0].output,
            Some((at as u64) / 60_000 * 60),
            "the clock is the rendered cutoff, minute-aligned"
        );
        assert_ne!(
            clock[0].output,
            task_ledger_clock(&[row("a", false, at + 60_000)], 0)[0].output,
            "a crossing minute moves the rendered cutoff"
        );
        assert_eq!(
            task_ledger_clock(&[bare], 0)[0].output,
            None,
            "a row with no window has no output clock"
        );
    }

    /// The section estimate charges the window exactly as it renders (spec
    /// #592): the same builder the body uses, so the card budget and the row
    /// cannot drift.
    #[test]
    fn the_estimate_covers_the_rendered_window() {
        let cut = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 5);
        let row = |window: Option<ShellOutputWindow>| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some("npm run build".into()),
            started_at: None,
            unconfirmed: false,
            activity: None,
            output: window,
        };
        let rows = |output: Option<ShellOutputWindow>| vec![row(output)];
        let window = ShellOutputWindow {
            text: (1..=15)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            clipped: true,
            captured_ms: cut,
        };

        let rendered = task_ledger_text(&rows(Some(window.clone())), 0).unwrap();
        let without = task_ledger_text(&rows(None), 0).unwrap();
        assert_eq!(
            task_ledger_estimate(&rows(Some(window))) - task_ledger_estimate(&rows(None)),
            rendered.len() - without.len(),
            "the estimate grows by exactly what the window renders"
        );
        // A long tail is reserved before the splitter runs: the estimate covers
        // the tail bytes, so a noisy shell cannot balloon the card past budget.
        let long = task_ledger_estimate(&rows(Some(ShellOutputWindow {
            text: "x".repeat(2000),
            clipped: true,
            captured_ms: cut,
        })));
        assert!(
            long >= task_ledger_estimate(&rows(None)) + 2000,
            "a 2000-byte tail is reserved"
        );
    }

    /// The pinned copy (ADR-0060, #412/#423/#501): the title is the count, the
    /// body one row per task — type noun, bolded label, start clock; a shell
    /// row's bare elapsed follows (`3m12s`), while a subagent row stops at its
    /// clock (its liveness is the activity fragment).
    #[test]
    fn the_section_renders_the_pinned_copy() {
        let start = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        // 127 s apart at 14:04:07, so the second row's clock is 14:04 and its
        // (unrendered) elapsed would be 1m05s at `now` (14:05:12).
        let second_start = start + 127_000;
        let now = start + 192_000;
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: Some("gh run watch".into()),
                started_at: Some(start),
                unconfirmed: false,
                activity: None,

                output: None,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                started_at: Some(second_start),
                unconfirmed: false,
                activity: None,

                output: None,
            },
        ];
        assert_eq!(task_ledger_title(&rows).unwrap(), "⏳ 后台任务（2）");
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "· shell：**gh run watch** · 14:02 · 3m12s\n· subagent：**review the diff** · 14:04"
        );
    }

    /// The four shape edges of a live row (ADR-0060 amendment, #423): the
    /// start clock ` · HH:MM` and the ` · XmYYs` elapsed arrive together only
    /// when the read carried a start, and a missing label drops its `：…` span
    /// whole — never a dangling colon or separator. The type word stays plain
    /// and only the label is bolded. A subagent row keeps its own noun and
    /// stops at the start clock: no total elapsed is ever rendered for it
    /// (spec #501).
    #[test]
    fn the_row_renders_its_four_shapes() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let now = at + 192_000; // 14:05:12, so the elapsed is 3m12s
        let row = |label: Option<&str>, started_at: Option<i64>| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: label.map(str::to_string),
            started_at,
            unconfirmed: false,
            activity: None,

            output: None,
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
        // The subagent keeps its own noun and no total elapsed.
        assert_eq!(
            task_ledger_text(
                &[TaskLedgerRow {
                    kind: TaskKind::Subagent,
                    label: Some("review the diff".into()),
                    started_at: Some(at),
                    unconfirmed: false,
                    activity: None,

                    output: None,
                }],
                now,
            )
            .unwrap(),
            "· subagent：**review the diff** · 14:02"
        );
    }

    /// A subagent row's child liveness (spec #501) renders as the front task
    /// panel's own fragment, appended after the start clock — a subagent row
    /// never renders a total elapsed: the running tool and its age (`bash 5s`),
    /// the phase and its age, then the wait — and the unconfirmed marker still
    /// trails LAST. A row the reconciliation could not confirm renders NO
    /// activity, whatever fragment it stores.
    #[test]
    fn a_rows_activity_fragment_follows_its_clock() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 4);
        let now = at + 65_000; // 14:05:05
        let row = |activity: Option<TaskLiveness>, unconfirmed: bool| TaskLedgerRow {
            kind: TaskKind::Subagent,
            label: Some("review the diff".into()),
            started_at: Some(at),
            unconfirmed,
            activity,
            output: None,
        };
        let render = |row: &TaskLedgerRow| task_ledger_text(std::slice::from_ref(row), now).unwrap();

        // The fragment-less shape: label, start clock, and no total runtime.
        assert_eq!(
            render(&row(None, false)),
            "· subagent：**review the diff** · 14:04"
        );

        let tool = TaskLiveness {
            activity: ChildActivity::Tool {
                name: "bash".into(),
                started_at: Some(now - 5_000),
            },
            last_activity_ms: now - 5_000,
            wait: None,
        };
        assert_eq!(
            render(&row(Some(tool), false)),
            "· subagent：**review the diff** · 14:04 · bash 5s"
        );

        let thinking = TaskLiveness {
            activity: ChildActivity::Thinking,
            last_activity_ms: now - 30_000,
            wait: Some(crate::feishu::card::AwaitingAction::Permission),
        };
        assert_eq!(
            render(&row(Some(thinking.clone()), false)),
            "· subagent：**review the diff** · 14:04 · 思考中 30s · 等待你的授权"
        );
        assert_eq!(
            render(&row(Some(thinking), true)),
            "· subagent：**review the diff** · 14:04 · ⚠️ 状态待确认",
            "an unconfirmed row renders no activity at all"
        );
        // A tool whose server clock has not landed shows its name alone, and a
        // row with no start still appends the fragment after the label.
        let untimed = TaskLiveness {
            activity: ChildActivity::Tool {
                name: "bash".into(),
                started_at: None,
            },
            last_activity_ms: now,
            wait: None,
        };
        assert_eq!(
            render(&row(Some(untimed), false)),
            "· subagent：**review the diff** · 14:04 · bash"
        );
        // The tool name is transcript text inside the markdown body: its own
        // emphasis characters are neutered like a label's.
        let starry = TaskLiveness {
            activity: ChildActivity::Tool {
                name: "we*bash_2".into(),
                started_at: None,
            },
            last_activity_ms: now,
            wait: None,
        };
        assert_eq!(
            render(&row(Some(starry), false)),
            "· subagent：**review the diff** · 14:04 · we&#42;bash&#95;2"
        );
        // A shell row renders its own elapsed and none of this: its activity
        // field stays None.
        assert_eq!(
            task_ledger_text(
                &[TaskLedgerRow {
                    kind: TaskKind::Shell,
                    label: None,
                    started_at: Some(at),
                    unconfirmed: false,
                    activity: None,

                    output: None,
                }],
                now,
            )
            .unwrap(),
            "· shell · 14:04 · 1m05s"
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
                activity: None,

                output: None,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: None,
                started_at: None,
                unconfirmed: false,
                activity: None,

                output: None,
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
            activity: None,

            output: None,
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
            output: None,
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
            output: None,
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

    /// Spec #588 / #590: the manual cleanup's entry names the ending as the
    /// user's, not the runtime's or a Wake's — the broom replaces the completion
    /// bell and the title marks the ending 人工 — while the fold body keeps the
    /// ordinary identity/timing shape.
    #[test]
    fn a_cleaned_entry_names_the_manual_ending() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let shell_entry = TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some("gh run watch".into()),
            id: Some("sh_abc".into()),
            started_at: Some(finished - 12 * 60_000),
            finished_at: Some(finished),
            ending: TaskEnding::Cleaned,
            output: None,
        };
        assert_eq!(
            task_entry_title(&shell_entry),
            "🧹 shell 已清理：gh run watch（人工）"
        );
        assert_eq!(task_entry_body(&shell_entry), "shell sh_abc · 14:02 · 12m");
        // A label-less cleanup still says who ended it.
        let bare = TaskCompletionEntry {
            kind: TaskKind::Subagent,
            label: None,
            id: Some("ses_child".into()),
            started_at: None,
            finished_at: Some(finished),
            ending: TaskEnding::Cleaned,
            output: None,
        };
        assert_eq!(task_entry_title(&bare), "🧹 subagent 已清理（人工）");
        assert_eq!(task_entry_body(&bare), "subagent ses_child · 14:02");
        // The other endings keep their bell and carry no manual marker.
        let wake = TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some("gh run watch".into()),
            id: None,
            started_at: None,
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
            output: None,
        };
        assert_eq!(task_entry_title(&wake), "🔔 shell 完成：gh run watch");
    }

    /// The cleanup button (spec #588, #590): the pinned copy, the handler's
    /// action, and the session the click works on — the card layer owns the
    /// shape so the button and the handler cannot drift.
    #[test]
    fn the_cleanup_button_carries_the_pinned_copy() {
        let button = cleanup_button("ses_1");
        assert_eq!(button["tag"], "button");
        assert_eq!(button["text"]["tag"], "plain_text");
        assert_eq!(button["text"]["content"], "清理待确认任务");
        assert_eq!(button["value"]["action"], "cleanup");
        assert_eq!(button["value"]["session_id"], "ses_1");
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
            activity: None,

            output: None,
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
                activity: None,

                output: None,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some(String::new()),
                started_at: None,
                unconfirmed: false,
                activity: None,

                output: None,
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
            activity: None,

            output: None,
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
            activity: None,

            output: None,
        }];
        assert!(
            task_ledger_text(&rows, 1_000).unwrap().ends_with(" · 0m00s"),
            "a skewed clock must not render a negative age"
        );
    }

    /// The render clock records every rendered number in whole SECONDS — a
    /// shell row's elapsed and any row's activity fragment age (ADR-0060, spec
    /// #501): the Bridge compares it at the cadence each path owes, a subagent
    /// row never clocks its total runtime, a row with neither number has an
    /// all-`None` clock, and a future start clamps like the render it keys.
    #[test]
    fn the_render_clock_reads_whole_seconds() {
        let start = 1_800_000_000_000;
        let activity_row = |unconfirmed: bool| TaskLedgerRow {
            kind: TaskKind::Subagent,
            label: None,
            started_at: Some(start),
            unconfirmed,
            activity: Some(TaskLiveness {
                activity: ChildActivity::Tool {
                    name: "bash".into(),
                    started_at: Some(start + 7_000),
                },
                last_activity_ms: start + 7_000,
                wait: None,
            }),

            output: None,
        };
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: None,
                started_at: Some(start),
                unconfirmed: false,
                activity: None,

                output: None,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: None,
                started_at: None,
                unconfirmed: false,
                activity: None,

                output: None,
            },
            activity_row(false),
        ];

        let clock = task_ledger_clock(&rows, start + 59_000);
        assert_eq!(
            clock,
            vec![
                LedgerRowClock {
                    elapsed: Some(59),
                    activity: None,

                    output: None,
                },
                LedgerRowClock {
                    elapsed: None,
                    activity: None,

                    output: None,
                },
                LedgerRowClock {
                    // The subagent's start is not rendered as an elapsed, so
                    // it has no elapsed clock — only its fragment's age ticks.
                    elapsed: None,
                    activity: Some(52),

                    output: None,
                },
            ],
            "the rendered seconds, in row order"
        );
        assert_eq!(
            task_ledger_clock(&rows, start + 60_000)[0].elapsed,
            Some(60),
            "the shell's elapsed seconds keep moving through a minute"
        );
        assert_eq!(
            task_ledger_clock(&rows, start - 5_000)[2],
            LedgerRowClock {
                elapsed: None,
                activity: Some(0),

                output: None,
            },
            "a future activity start clamps like the render"
        );
        // An unconfirmed row renders no activity, so that age never ticks —
        // the row's clock carries only what the card actually shows.
        assert_eq!(
            task_ledger_clock(&[activity_row(true)], start + 59_000)[0],
            LedgerRowClock {
                elapsed: None,
                activity: None,

                output: None,
            }
        );
        assert_eq!(task_ledger_clock(&[], start), Vec::<LedgerRowClock>::new());
    }

    /// A subagent row renders no total elapsed (spec #501 format): the row
    /// stops at its start clock, or carries its child's activity fragment, and
    /// its clock never includes a total runtime.
    #[test]
    fn a_subagent_row_renders_no_total_elapsed() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let now = at + 192_000; // 14:05:12, a 3m12s runtime
        let row = |kind: TaskKind, activity: Option<TaskLiveness>| TaskLedgerRow {
            kind,
            label: Some("review the diff".into()),
            started_at: Some(at),
            unconfirmed: false,
            activity,
            output: None,
        };
        assert_eq!(
            task_ledger_text(&[row(TaskKind::Shell, None)], now).unwrap(),
            "· shell：**review the diff** · 14:02 · 3m12s",
            "a shell row keeps its elapsed: it has no activity fragment"
        );
        assert_eq!(
            task_ledger_text(&[row(TaskKind::Subagent, None)], now).unwrap(),
            "· subagent：**review the diff** · 14:02",
            "a fragment-less subagent row stops at its start clock"
        );
        let activity = TaskLiveness {
            activity: ChildActivity::Tool {
                name: "bash".into(),
                started_at: Some(now - 5_000),
            },
            last_activity_ms: now - 5_000,
            wait: None,
        };
        assert_eq!(
            task_ledger_text(&[row(TaskKind::Subagent, Some(activity))], now).unwrap(),
            "· subagent：**review the diff** · 14:02 · bash 5s",
            "a subagent row's liveness is its child's fragment, not a total"
        );
        // The clock follows the render: only the fragment's age ticks, so the
        // subagent's runtime can never owe a flush.
        let clocks = task_ledger_clock(&[row(TaskKind::Subagent, None)], now);
        assert_eq!(clocks[0].elapsed, None);
        assert_eq!(clocks[0].activity, None);
    }

    /// An untimed tool renders its name alone, so its age never ticks.
    #[test]
    fn an_untimed_tool_has_no_activity_clock() {
        let rows = vec![TaskLedgerRow {
            kind: TaskKind::Subagent,
            label: None,
            started_at: None,
            unconfirmed: false,
            activity: Some(TaskLiveness {
                activity: ChildActivity::Tool {
                    name: "bash".into(),
                    started_at: None,
                },
                last_activity_ms: 1_000,
                wait: None,
            }),

            output: None,
        }];
        assert_eq!(
            task_ledger_clock(&rows, 99_000)[0],
            LedgerRowClock {
                elapsed: None,
                activity: None,

                output: None,
            }
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
            unconfirmed: false,
            activity: None,

            output: None,
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
        // A row that renders an activity fragment reserves room for it, sized
        // from the fragment's own parts (label, age, wait).
        let plain = TaskLedgerRow {
            kind: TaskKind::Subagent,
            label: None,
            started_at: None,
            unconfirmed: false,
            activity: None,

            output: None,
        };
        let with_activity = |wait: Option<crate::feishu::card::AwaitingAction>| TaskLedgerRow {
            kind: TaskKind::Subagent,
            label: None,
            started_at: None,
            unconfirmed: false,
            activity: Some(TaskLiveness {
                activity: ChildActivity::Thinking,
                last_activity_ms: 0,
                wait,
            }),

            output: None,
        };
        let untimed = TaskLedgerRow {
            kind: TaskKind::Subagent,
            label: None,
            started_at: None,
            unconfirmed: false,
            activity: Some(TaskLiveness {
                activity: ChildActivity::Tool {
                    name: "bash".into(),
                    started_at: None,
                },
                last_activity_ms: 0,
                wait: None,
            }),

            output: None,
        };
        assert!(
            task_ledger_estimate(&[with_activity(None)]) > task_ledger_estimate(std::slice::from_ref(&plain)),
            "an activity fragment adds its own bytes"
        );
        assert!(
            task_ledger_estimate(&[with_activity(Some(
                crate::feishu::card::AwaitingAction::Permission
            ))]) > task_ledger_estimate(&[with_activity(None)]),
            "a wait's words are reserved too"
        );
        assert!(
            task_ledger_estimate(&[untimed]) > task_ledger_estimate(std::slice::from_ref(&plain)),
            "an untimed tool still reserves its name"
        );
        // The fragment's reserve covers what the row renders: the label's
        // entity escaping (`*`/`_`/`&` expand three- to fivefold) and the real
        // ` · ` separators — measured through the renderer's own parts, so a
        // long markdown-heavy tool name cannot outgrow the reserve.
        let widest_age = TaskLiveness {
            activity: ChildActivity::Tool {
                name: "a*b_c&d".into(),
                started_at: Some(0),
            },
            last_activity_ms: 0,
            wait: Some(crate::feishu::card::AwaitingAction::Both),
        };
        let now = 3_599_000; // the age renders `59m59s`, the widest shape
        assert_eq!(
            activity_estimate(&widest_age),
            escaped_entities(&widest_age.title_fragment(now)).len(),
            "the reserve covers the rendered fragment's escaped bytes exactly"
        );
        // Every shape the row can render is covered at every age width: the
        // estimate never undercounts the escaped fragment.
        for activity in [
            widest_age.clone(),
            TaskLiveness {
                activity: ChildActivity::Tool {
                    name: "bash".into(),
                    started_at: None,
                },
                last_activity_ms: 0,
                wait: Some(crate::feishu::card::AwaitingAction::Permission),
            },
            TaskLiveness {
                activity: ChildActivity::Thinking,
                last_activity_ms: 0,
                wait: None,
            },
            TaskLiveness {
                activity: ChildActivity::Reasoning,
                last_activity_ms: 0,
                wait: Some(crate::feishu::card::AwaitingAction::Both),
            },
            TaskLiveness {
                activity: ChildActivity::Replying,
                last_activity_ms: 0,
                wait: None,
            },
        ] {
            for now in [0, 5_000, 90_000, 3_599_000] {
                let rendered = escaped_entities(&activity.title_fragment(now)).len();
                assert!(
                    activity_estimate(&activity) >= rendered,
                    "the reserve covers {rendered} rendered bytes for {activity:?} at {now}"
                );
            }
        }
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
            output: None,
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
            output: None,
        };
        assert_eq!(task_entry_title(&subagent), "🔔 subagent 完成：review the diff");
        assert_eq!(task_entry_body(&subagent), "subagent ses_child · 14:02 · 1m");
    }

    /// Spec #588 / #593: a shell's completion entry carries the result — the
    /// output tail under the identity line, labelled with the same pinned copy
    /// the live window uses (截至于 HH:MM, plus 仅最后 N 行 · 已截断 when the
    /// record held more).
    #[test]
    fn a_shell_entries_fold_body_carries_the_output_tail() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let captured = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 5);
        let entry = |output: Option<TaskOutput>| TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some("gh run watch".into()),
            id: Some("sh_abc".into()),
            started_at: Some(finished - 12 * 60_000),
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
            output,
        };
        let window = |text: &str, clipped: bool| {
            TaskOutput::Window(ShellOutputWindow {
                text: text.into(),
                clipped,
                captured_ms: captured,
            })
        };

        assert_eq!(
            task_entry_body(&entry(Some(window("line 1\nline 2", false)))),
            "shell sh_abc · 14:02 · 12m\n  截至于 14:05\n```\nline 1\nline 2\n```",
            "the tail follows the identity line, labelled and fenced"
        );
        assert_eq!(
            task_entry_body(&entry(Some(window("a\nb\nc\nd", true)))),
            "shell sh_abc · 14:02 · 12m\n  截至于 14:05 · 仅最后 4 行 · 已截断\n```\na\nb\nc\nd\n```",
            "a clipped window names its lines"
        );
        assert_eq!(
            task_entry_body(&entry(None)),
            "shell sh_abc · 14:02 · 12m",
            "no output fact renders the identity line alone"
        );
    }

    /// Spec #588 / #593: a record that cannot be read says so in place of the
    /// tail, while the endings with nothing to show stay identity-only whatever
    /// the entry carries — the 已失联 entry has no record, the 🧹 cleanup is
    /// the user's own ending, and a subagent has no shell output.
    #[test]
    fn an_unreadable_record_says_output_unavailable() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let shell =
            |ending: TaskEnding, finished_at: Option<i64>, output: Option<TaskOutput>| TaskCompletionEntry {
                kind: TaskKind::Shell,
                label: Some("gh run watch".into()),
                id: Some("sh_abc".into()),
                started_at: Some(finished - 12 * 60_000),
                finished_at,
                ending,
                output,
            };
        let window = TaskOutput::Window(ShellOutputWindow {
            text: "late output".into(),
            clipped: false,
            captured_ms: finished,
        });

        assert_eq!(
            task_entry_body(&shell(
                TaskEnding::Wake {
                    state: Some("error".into()),
                },
                Some(finished),
                Some(TaskOutput::Unavailable),
            )),
            "shell sh_abc · 14:02 · 12m\n  输出已不可用",
            "a failed or vanished record says so, never an empty panel"
        );
        assert_eq!(
            task_entry_body(&shell(TaskEnding::Lost, None, Some(window.clone()))),
            "shell sh_abc",
            "the 已失联 entry has no completion clock and no record to show"
        );
        assert_eq!(
            task_entry_body(&shell(TaskEnding::Lost, Some(finished), Some(window.clone()))),
            "shell sh_abc · 14:02 · 12m",
            "the 已失联 ending never shows output, whatever the entry carries"
        );
        assert_eq!(
            task_entry_body(&shell(TaskEnding::Cleaned, Some(finished), Some(window.clone()))),
            "shell sh_abc · 14:02 · 12m",
            "the 🧹 cleanup entry is identity-only"
        );

        let subagent = TaskCompletionEntry {
            kind: TaskKind::Subagent,
            label: Some("review the diff".into()),
            id: Some("ses_child".into()),
            started_at: None,
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
            output: Some(window),
        };
        assert_eq!(
            task_entry_body(&subagent),
            "subagent ses_child · 14:02",
            "a subagent entry has no shell output"
        );
    }

    /// Spec #588 / #593: the entry's estimate charges its output tail exactly
    /// as the body renders it — the same block builder — so a noisy shell's
    /// completion entry is reserved before the card splitter runs and cannot
    /// balloon the budget.
    #[test]
    fn the_entry_estimate_covers_the_output_tail() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let captured = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 5);
        let entry = |output: Option<TaskOutput>| TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some("gh run watch".into()),
            id: Some("sh_abc".into()),
            started_at: Some(finished - 12 * 60_000),
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
            output,
        };
        let window = |text: String| {
            TaskOutput::Window(ShellOutputWindow {
                text,
                clipped: true,
                captured_ms: captured,
            })
        };
        let tail = (1..=15)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let with_tail = entry(Some(window(tail)));
        let without = entry(None);
        assert_eq!(
            task_entry_estimate(&with_tail) - task_entry_estimate(&without),
            task_entry_body(&with_tail).len() - task_entry_body(&without).len(),
            "the estimate grows by exactly what the tail renders"
        );
        let long = entry(Some(window("x".repeat(2000))));
        assert!(
            task_entry_estimate(&long) >= task_entry_estimate(&without) + 2000,
            "a 2000-byte tail is reserved"
        );
        let unavailable = entry(Some(TaskOutput::Unavailable));
        assert_eq!(
            task_entry_estimate(&unavailable) - task_entry_estimate(&without),
            task_entry_body(&unavailable).len() - task_entry_body(&without).len(),
            "the unavailable line is reserved too"
        );
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
                output: None,
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
            output: None,
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
                output: None,
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
            output: None,
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
