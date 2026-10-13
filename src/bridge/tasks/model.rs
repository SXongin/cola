//! The Background Task read model (ADR-0079): the data a **Background Task
//! State** module owns and the **Background Task Ledger** renders.
//!
//! These are the type DEFINITIONS only — the rows the ledger renders, the
//! completion entries a read owes, and the clocks and the change decision a
//! refresh compares. Every title/body/estimate/format function over them, and
//! the pinned copy, stay in `src/feishu/card/ledger.rs`, which re-exports these
//! types so no caller's import path breaks.

use serde::{Deserialize, Serialize};

use crate::backend::{BackgroundTask, ShellOutputWindow, Wake};
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

/// The stable identity of one **Background Task** run (ADR-0080), and the key
/// its **Task Ending**s are recorded under.
///
/// The two sides that observe a run name it differently: a read still carrying
/// the launch names the launching tool call's id, while a **Wake** arriving
/// after the launch left the read (compaction, the V2 page cap) names only the
/// shell/child id it reports — never an invented one. The identity therefore
/// carries EVERY id the run is known by over one stored [`Self::key`], so a
/// writer resolves against it (an exact key, an exact Wake id, else the newest
/// record naming any other id it holds) instead of minting a second record for
/// a run that already has one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskRunId {
    /// The id the run's record is stored under: the launch's call id when the
    /// writer holds one, else the shell/child id a Wake itself reports.
    pub(crate) key: String,
    /// The launching tool call's id, when the writer holds the launch.
    #[serde(default)]
    pub(crate) call_id: Option<String>,
    /// The shell the run executes in, when known.
    #[serde(default)]
    pub(crate) shell_id: Option<String>,
    /// The child session a subagent runs in, when known.
    #[serde(default)]
    pub(crate) child_id: Option<String>,
    /// The Wake whose completion this run recorded, when known.
    #[serde(default)]
    pub(crate) wake_id: Option<String>,
}

impl TaskRunId {
    /// The run a read that carries the LAUNCH names (ADR-0080): the launch's
    /// own tool call id is the stored key — the one id every writer of that
    /// read agrees on — and the ids it carries ride along as aliases.
    // `#[allow(dead_code)]`: nothing derives an ending yet in this slice (spec
    // #699, slice 1) — the recording writers (#701/#702) resolve runs with this.
    #[allow(dead_code)]
    pub(crate) fn of_launch(task: &BackgroundTask) -> Self {
        let call_id = task.tool.call_id.clone();
        Self {
            key: call_id.clone(),
            call_id: Some(call_id),
            shell_id: task.shell_id.clone(),
            child_id: task.child_id.clone(),
            wake_id: None,
        }
    }

    /// The run a **Wake** names on its own (ADR-0080): the shell id it reports,
    /// else the job it reports, else the child session — keyed by that same id.
    /// `None` when the Wake names nothing at all: the identity is never
    /// invented.
    // `#[allow(dead_code)]`: nothing derives an ending yet in this slice (spec
    // #699, slice 1) — the Wake path (#702) names its run with this.
    #[allow(dead_code)]
    pub(crate) fn of_wake(wake: &Wake) -> Option<Self> {
        let key = wake
            .shell_id
            .clone()
            .or_else(|| wake.job_id.clone())
            .or_else(|| wake.child_id.clone())?;
        Some(Self {
            key,
            call_id: None,
            shell_id: wake.shell_id.clone(),
            child_id: wake.child_id.clone(),
            wake_id: Some(wake.id.as_str().to_string()),
        })
    }

    /// Every id this run is known by, the stored key first — what a resolution
    /// matches a stored record's own ids against.
    pub(crate) fn ids(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.key.as_str())
            .chain(self.call_id.as_deref())
            .chain(self.shell_id.as_deref())
            .chain(self.child_id.as_deref())
            .chain(self.wake_id.as_deref())
    }

    /// Whether this run is known by `id` — its stored key or any alias it holds.
    pub(crate) fn names(&self, id: &str) -> bool {
        self.ids().any(|held| held == id)
    }

    /// Fold every id `other` holds into this run (ADR-0080): a record carries
    /// EVERY id its run is known by, so a later writer naming the run another
    /// way resolves by alias instead of minting a second record. The stored key
    /// never moves, and an id already known — the reused-id residual the
    /// newest-match rule already accepts — is left as it was.
    pub(crate) fn absorb(&mut self, other: &TaskRunId) {
        for (known, held) in [
            (&mut self.call_id, &other.call_id),
            (&mut self.shell_id, &other.shell_id),
            (&mut self.child_id, &other.child_id),
            (&mut self.wake_id, &other.wake_id),
        ] {
            if known.is_none() {
                *known = held.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MessageId, ToolIdentity, WakeSource};

    /// A launch the read still carries: the type's own identity names the tool
    /// call that started it.
    fn launch(call_id: &str, shell_id: Option<&str>, child_id: Option<&str>) -> BackgroundTask {
        BackgroundTask {
            tool: ToolIdentity {
                name: "shell".into(),
                call_id: call_id.into(),
            },
            shell_id: shell_id.map(str::to_string),
            child_id: child_id.map(str::to_string),
            started_at: Some(1_000),
        }
    }

    /// A Wake naming whatever ids the payload carried.
    fn wake(shell_id: Option<&str>, job_id: Option<&str>, child_id: Option<&str>) -> Wake {
        Wake {
            id: MessageId::new("msg_wake_1"),
            created_ms: Some(2_000),
            source: WakeSource::Shell,
            shell_id: shell_id.map(str::to_string),
            job_id: job_id.map(str::to_string),
            child_id: child_id.map(str::to_string),
            state: Some("completed".into()),
            label: None,
        }
    }

    #[test]
    fn a_launch_names_the_run_by_its_tool_call() {
        let id = TaskRunId::of_launch(&launch("call_1", Some("shell_1"), None));
        assert_eq!(id.key, "call_1", "the launch's own call id is the stored key");
        assert!(id.names("call_1"));
        assert!(id.names("shell_1"), "the shell id the launch carries is an alias");
        assert!(!id.names("shell_2"));
        assert_eq!(id.wake_id, None, "a launch records no Wake");

        let subagent = TaskRunId::of_launch(&launch("call_2", None, Some("ses_child")));
        assert_eq!(subagent.key, "call_2");
        assert!(subagent.names("ses_child"));
    }

    /// A Wake that arrives after the launch left the read names the run by the
    /// id IT reports — never an invented one, and `None` when it names nothing.
    #[test]
    fn a_wake_names_the_run_only_by_an_id_it_reports() {
        let shell = TaskRunId::of_wake(&wake(Some("shell_1"), None, None)).expect("the shell id names it");
        assert_eq!(shell.key, "shell_1");
        assert_eq!(shell.wake_id.as_deref(), Some("msg_wake_1"));
        assert_eq!(shell.call_id, None, "a launchless Wake invents no call id");

        let job = TaskRunId::of_wake(&wake(None, Some("job_1"), None)).expect("the job names it");
        assert_eq!(job.key, "job_1", "the job stands in for the shell id");

        let child = TaskRunId::of_wake(&wake(None, None, Some("ses_child"))).expect("the child names it");
        assert_eq!(child.key, "ses_child");

        assert_eq!(
            TaskRunId::of_wake(&wake(None, None, None)),
            None,
            "a Wake naming no id names no run"
        );
        assert_eq!(
            TaskRunId::of_wake(&wake(Some("shell_1"), Some("job_1"), Some("ses_child")))
                .expect("shell first")
                .key,
            "shell_1"
        );
    }

    /// A record carries EVERY id its run is known by: a later writer holding
    /// another spelling is folded in rather than minting a second record, and
    /// the stored key never moves.
    #[test]
    fn absorbing_a_later_writer_keeps_the_key_and_adds_the_ids() {
        let mut stored = TaskRunId::of_wake(&wake(Some("shell_1"), None, None)).expect("the wake names it");
        stored.absorb(&TaskRunId::of_launch(&launch("call_1", Some("shell_1"), None)));
        assert_eq!(
            stored.key, "shell_1",
            "the stored key is the run's own, not the newcomer's"
        );
        assert_eq!(stored.call_id.as_deref(), Some("call_1"));
        assert!(stored.names("call_1") && stored.names("shell_1") && stored.names("msg_wake_1"));
    }
}
