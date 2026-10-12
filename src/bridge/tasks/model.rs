//! The Background Task read model (ADR-0079): the data a **Background Task
//! State** module owns and the **Background Task Ledger** renders.
//!
//! These are the type DEFINITIONS only — the rows the ledger renders, the
//! completion entries a read owes, and the clocks and the change decision a
//! refresh compares. Every title/body/estimate/format function over them, and
//! the pinned copy, stay in `src/feishu/card/ledger.rs`, which re-exports these
//! types so no caller's import path breaks.

use crate::backend::ShellOutputWindow;
use crate::feishu::card::tool_render::TaskLiveness;

/// The kind of Background Task a ledger row names. Only these two background
/// through the V2 tool shape (ADR-0059/0060).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Subagent,
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

/// One shell completion entry's output fact (spec #588, ticket #593): the
/// result its fold body carries under the identity line. The Bridge reads it
/// once, when the entry renders — [`TaskOutput::Unavailable`] when the record
/// was gone or the read failed, so the body says 「输出已不可用」 rather than
/// posing an empty panel as output; a successful empty capture carries no fact
/// at all, so the entry stays identity-only (spec #588, review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutput {
    /// The record answered with the tail to show: the same bounded window the
    /// live row renders ([`ShellOutputWindow`]), carrying only its truncation
    /// label (仅最后 N 行 · 已截断) when the capture held more — never the live
    /// row's 截至于 HH:MM read clock: the identity line already carries the
    /// run's own timing and a completed capture cannot grow (review, PR #595).
    /// Never an empty window — the readable-empty capture leaves the entry's
    /// output `None`.
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
/// ending that retired it without a Wake) and hands them over; the ledger keeps
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
    /// the last bounded lines with their 已截断 label (never the live row's
    /// 截至于 read clock) after the identity line, or
    /// [`TaskOutput::Unavailable`]'s 「输出已不可用」 when the
    /// record could not be read. `None` when the ending shows no output (a
    /// subagent, the 已失联 ending, the 🧹 cleanup) or when no read was spent,
    /// so the body stays identity-only.
    pub output: Option<TaskOutput>,
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

/// The granularity at which a ledger read compares its clock against what the
/// card last rendered (ADR-0060). The stored clock is the rendered elapsed in
/// whole seconds; the cadence decides how much of it must move to owe a flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LedgerCadence {
    /// Whole minutes — the live render and its Wake handover, whose flushes are
    /// content-driven: a per-render second clock would owe a flush on nearly
    /// every render. A read inside the same rendered minute owes nothing.
    Minute,
    /// Whole seconds — the yielded refresh: the ~8 s Session Sync reads are a
    /// waiting card's only clock, so the moment a row's rendered second moves,
    /// the card owes the PATCH (≈0.125 QPS per waiting card, far below
    /// Feishu's per-message cap).
    Second,
}

/// What a ledger refresh moved (#457), split so the live render can tell real
/// work from clock churn: `rows` — a visible fact moved (membership, a row's
/// label or status, the fragment's words, the output window's text) — is
/// progress; `clock` — only a rendered elapsed, a fragment's age or the output
/// window's cutoff crossed the path's cadence — owes the flush but is not new
/// work. Both halves flush the card; the external renderer's idle bound renews
/// on `rows` alone, or a silent task's ticking age would keep its card live
/// forever.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LedgerChange {
    pub(crate) rows: bool,
    pub(crate) clock: bool,
}

impl LedgerChange {
    /// Whether the card owes a flush (either half moved).
    pub(crate) fn owes(self) -> bool {
        self.rows || self.clock
    }
}
