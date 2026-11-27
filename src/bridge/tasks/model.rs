//! The Background Task read model (ADR-0079): the data a **Background Task
//! State** module owns and the **Background Task Ledger** renders.
//!
//! These are the type DEFINITIONS only — the rows the ledger renders, the
//! completion entries a read owes, and the clocks and the change decision a
//! refresh compares. Every title/body/estimate/format function over them, and
//! the pinned copy, stay in `src/feishu/card/ledger.rs`, which re-exports these
//! types so no caller's import path breaks.

use crate::backend::runs::TaskRunIdentity;
use crate::backend::{BackgroundTask, ShellOutputWindow, Wake, WakeSource};
use crate::feishu::card::tool_view::TaskLiveness;

/// The kind of Background Task a ledger row names: the two the V2 tool shape
/// derives (ADR-0059/0060), plus the job kinds a job-sourced task carries as
/// its tool name and a job completion Wake names (ADR-0118).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Subagent,
    CodeMode,
    Image,
    Video,
}

impl TaskKind {
    /// The neutral token this kind renders as: the tool's own (`shell` /
    /// `subagent`, ADR-0107) or the job's kind word (`codemode` / `image` /
    /// `video`, ADR-0118). One home for the word the live row, the entry body
    /// and a durable identity record, so they cannot drift apart.
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subagent => "subagent",
            Self::CodeMode => "codemode",
            Self::Image => "image",
            Self::Video => "video",
        }
    }

    /// Whether this kind's row renders a running total (ADR-0060, ADR-0118):
    /// a shell's elapsed is its only liveness, and a job's row shows its total
    /// runtime the same way. A subagent row never does — its liveness is its
    /// child's activity fragment, and a total would only restate the
    /// fragment's age.
    pub(crate) fn renders_elapsed(self) -> bool {
        matches!(self, Self::Shell | Self::CodeMode | Self::Image | Self::Video)
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
    /// The client itself reported the run `Failed`; its child's terminal
    /// evidence supplied the clock. The entry reads 失败, not the runtime's
    /// generic 结束.
    ClientFailed,
    /// The client itself reported the run `Cancelled`; its child's terminal
    /// evidence supplied the clock. The entry reads 已取消.
    ClientCancelled,
}

/// A **Child Job**'s owner, as its ledger row marks it (ADR-0118): the owning
/// subagent row's label when one in the same read names that child, or the
/// neutral fallback when none does. A task that is not a child job carries no
/// owner at all, so its row never renders a `↳` marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildJobOwner {
    /// The owning subagent row's label.
    Named(String),
    /// No row in this read names the child: the ledger renders the pinned
    /// `↳ 子会话` fallback.
    Unnamed,
}

/// One live Background Task as its ledger row renders it: the task's type, the
/// label joined from the originating tool part's input by `call_id` (`None`
/// when that input names no label — the row then renders bare), when the run
/// started (`None` renders no start clock; a shell or job row also renders its
/// elapsed from it, a subagent row never does), whether a runtime
/// reconciliation could not confirm it as running (issue #454 — the row then
/// carries the 状态待确认 marker, and only its own Wake or a positive terminal
/// verdict can retire it), the **Child Job** owner it marks itself with
/// (ADR-0118), and the task's child-session liveness when it has one (spec
/// #501 — a background `subagent` row carries the same activity fragment the
/// live `task` panel shows, while a `shell` or job row has none).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLedgerRow {
    pub kind: TaskKind,
    pub label: Option<String>,
    pub started_at: Option<i64>,
    /// True when a reconciliation read reported the task not running while no
    /// Wake retired it. The row stays live (the runtime read is evidence, not
    /// an ending) but reads as unconfirmed.
    pub unconfirmed: bool,
    /// The **Child Job** owner this row marks itself with (ADR-0118): `None`
    /// for the session's own work, `Some` for a job a descendant session owns
    /// — the owner's label when a row in the same read names that child, else
    /// [`ChildJobOwner::Unnamed`]. The ledger appends ` · ↳ <owner>` after the
    /// row's elapsed, so a descendant's work never reads as the parent's.
    pub child_job: Option<ChildJobOwner>,
    /// The child session's last successfully gathered liveness (spec #501),
    /// rendered as the front task panel's own fragment — `bash 5s`,
    /// `waiting 1m30s · 等待你的授权` — after the row's start clock. `None` for a
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

/// The identity a just-derived ending records (ADR-0080): the launch's own
/// facts, plus the label its originating tool input names — or the label the
/// retiring **Agent Run** already carries (`label_override`), so a completion
/// entry names the run it belongs to even when a multi-child launch or a
/// compacted read cannot re-join the input. Captured when the ending is
/// derived, so a later card's entry is immune to compaction and to a truncated
/// read.
pub(crate) fn identity_of_task(
    task: &BackgroundTask,
    input: Option<&serde_json::Value>,
    label_override: Option<&str>,
) -> TaskRunIdentity {
    let tool = task.tool.name.clone();
    TaskRunIdentity {
        tool: Some(tool.clone()),
        call_id: Some(task.tool.call_id.clone()),
        shell_id: task.shell_id.clone(),
        child_id: task.child_id.clone(),
        started_at: task.started_at,
        label: label_override
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string)
            .or_else(|| task_label(task_kind(task), input))
            // A job-sourced task's launching part is joined by `metadata.jobID`
            // upstream, not by its call id, so the task's own neutral label is
            // where its facts live (ADR-0118).
            .or_else(|| task.label.clone().filter(|label| !label.trim().is_empty())),
    }
}

/// The identity a run's **Wake** ending is recorded with when the read no longer
/// carries the launch (ADR-0080): what the Wake itself names — the tool its
/// source implies, the shell/child id, and the label its own text carried. The
/// call id stays `None`: a Wake never carried one, and the record's stored key
/// (the shell/child id it reports) stands in for it.
pub(crate) fn identity_of_wake(wake: &Wake) -> TaskRunIdentity {
    let tool = match &wake.source {
        WakeSource::Subagent => "subagent",
        source => wake_task_kind(source).map(TaskKind::token).unwrap_or("shell"),
    };
    TaskRunIdentity {
        tool: Some(tool.to_string()),
        call_id: None,
        shell_id: wake.shell_id.clone(),
        child_id: wake.child_id.clone(),
        started_at: None,
        label: wake.label.clone().filter(|label| !label.trim().is_empty()),
    }
}

/// The ledger kind of a Background Task. The **child fact** wins: a task that
/// carries a child session is a subagent, whatever it is called, so a third
/// adapter's own launch noun never picks the renderer (ADR-0105 §5). The one
/// identity-less shape is a V2 `subagent` launch whose metadata omitted its
/// session id: there the adapter's neutral launch noun — never a vendor id, and
/// exactly the ADR-0107 "name as a last fact" exception — is all that is left.
/// A job-sourced task carries its job kind as the tool name, and those map to
/// their own kind rather than falling through to a shell row (ADR-0118).
pub(crate) fn task_kind(task: &BackgroundTask) -> TaskKind {
    if task.child_id.is_some() || task.tool.name == "subagent" {
        return TaskKind::Subagent;
    }
    job_task_kind(&task.tool.name).unwrap_or(TaskKind::Shell)
}

/// The ledger kind a job's own `kind` word names (ADR-0118) — the value a
/// job-sourced task carries as its tool name. `None` for every other name: the
/// tool-part kinds keep their own derivation, and a kind this build does not
/// model keeps the tolerant shell default.
fn job_task_kind(kind: &str) -> Option<TaskKind> {
    match kind {
        "codemode" => Some(TaskKind::CodeMode),
        "image" => Some(TaskKind::Image),
        "video" => Some(TaskKind::Video),
        _ => None,
    }
}

/// The ledger kind a completion Wake's source names (ADR-0118): the tool
/// sources (`shell` / `subagent`), plus a job's own marker — `codemode`,
/// `generate_image`, `generate_video`. `None` for a source that completes no
/// Background Task: a restart, an interruption, or a marker this build does not
/// know, which stays a non-resumption (ADR-0066).
pub(crate) fn wake_task_kind(source: &WakeSource) -> Option<TaskKind> {
    match source {
        WakeSource::Shell => Some(TaskKind::Shell),
        WakeSource::Subagent => Some(TaskKind::Subagent),
        WakeSource::Other(marker) => match marker.as_str() {
            "codemode" => Some(TaskKind::CodeMode),
            "generate_image" => Some(TaskKind::Image),
            "generate_video" => Some(TaskKind::Video),
            _ => None,
        },
        WakeSource::Restart | WakeSource::Interrupt | WakeSource::Unknown => None,
    }
}

/// The `↳` marker a task's row carries (ADR-0118): `None` for the session's
/// own work; `Some` for a **Child Job**, carrying the owning subagent row's
/// label when the read resolved one and the neutral fallback otherwise.
pub(crate) fn child_job_owner(task: &BackgroundTask) -> Option<ChildJobOwner> {
    task.child_job
        .then(|| match task.owner_label.as_deref().map(str::trim) {
            Some(label) if !label.is_empty() => ChildJobOwner::Named(label.to_string()),
            _ => ChildJobOwner::Unnamed,
        })
}

/// The label the originating tool part's input names for a task of `kind`:
/// the shell's `command` (or its `description` when the payload carries no
/// command), the subagent's `description` — the `subagent` arm nothing else
/// needed. `None` when the input names no label, so the row renders bare
/// rather than inventing one (the completion entry's own rule).
///
/// A job kind has no such part: its launch joins by `metadata.jobID` and the
/// adapter already captured the label on the task ([`BackgroundTask::label`],
/// ADR-0118), so the input-keyed join names nothing here.
pub(crate) fn task_label(kind: TaskKind, input: Option<&serde_json::Value>) -> Option<String> {
    let label = match kind {
        TaskKind::Shell => input
            .and_then(|input| input.get("command"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                input
                    .and_then(|input| input.get("description"))
                    .and_then(serde_json::Value::as_str)
            }),
        TaskKind::Subagent => input
            .and_then(|input| input.get("description"))
            .and_then(serde_json::Value::as_str),
        TaskKind::CodeMode | TaskKind::Image | TaskKind::Video => None,
    }?;
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::runs::RunKey;
    use crate::backend::{MessageId, ToolIdentity, WakeSource};
    use crate::bridge::test_support::launch_task;

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
        let id = RunKey::of_launch(&launch_task("call_1", Some("shell_1"), None));
        assert_eq!(id.key, "call_1", "the launch's own call id is the stored key");
        assert!(id.names("call_1"));
        assert!(id.names("shell_1"), "the shell id the launch carries is an alias");
        assert!(!id.names("shell_2"));
        assert_eq!(id.wake_id, None, "a launch records no Wake");

        let subagent = RunKey::of_launch(&launch_task("call_2", None, Some("ses_child")));
        assert_eq!(subagent.key, "call_2");
        assert!(subagent.names("ses_child"));
    }

    /// A Wake that arrives after the launch left the read names the run by the
    /// id IT reports — never an invented one, and `None` when it names nothing.
    #[test]
    fn a_wake_names_the_run_only_by_an_id_it_reports() {
        let shell = RunKey::of_wake(&wake(Some("shell_1"), None, None)).expect("the shell id names it");
        assert_eq!(shell.key, "shell_1");
        assert_eq!(shell.wake_id.as_deref(), Some("msg_wake_1"));
        assert_eq!(shell.call_id, None, "a launchless Wake invents no call id");

        let job = RunKey::of_wake(&wake(None, Some("job_1"), None)).expect("the job names it");
        assert_eq!(job.key, "job_1", "the job stands in for the shell id");

        let child = RunKey::of_wake(&wake(None, None, Some("ses_child"))).expect("the child names it");
        assert_eq!(child.key, "ses_child");

        assert_eq!(
            RunKey::of_wake(&wake(None, None, None)),
            None,
            "a Wake naming no id names no run"
        );
        // A Wake reporting BOTH a shell id and a distinct job id names the run
        // by the shell (the stored key) but carries the job id as an alias
        // (ADR-0080: "the record carries every id the run is known by"), so a
        // later writer naming the run by the job id still resolves it.
        let both =
            RunKey::of_wake(&wake(Some("shell_1"), Some("job_1"), Some("ses_child"))).expect("shell first");
        assert_eq!(both.key, "shell_1");
        assert_eq!(
            both.job_id.as_deref(),
            Some("job_1"),
            "the job id the Wake reported rides as an alias"
        );
        assert!(
            both.names("job_1"),
            "a later writer holding the job id resolves the run"
        );
    }

    /// The ledger kind comes from the run's identity first — a child session,
    /// else a shell — so an adapter's own name cannot pick the renderer. A V2
    /// subagent launch with no session id is the identity-less shape, and there
    /// its neutral launch noun (never a vendor id) is the remaining fact.
    #[test]
    fn the_ledger_kind_reads_the_identity_not_the_name() {
        let task = |name: &str, child: Option<&str>, shell: Option<&str>| BackgroundTask {
            tool: crate::backend::ToolIdentity {
                name: name.into(),
                call_id: "call_1".into(),
            },
            shell_id: shell.map(str::to_string),
            child_id: child.map(str::to_string),
            started_at: None,
            ..Default::default()
        };
        let shell = task("spawn_agent", None, Some("sh_1"));
        assert_eq!(task_kind(&shell), TaskKind::Shell);
        assert_eq!(
            task_label(task_kind(&shell), Some(&serde_json::json!({"command": "ls"}))),
            Some("ls".to_string())
        );
        let child = task("spawn_agent", Some("ses_child"), None);
        assert_eq!(task_kind(&child), TaskKind::Subagent);
        assert_eq!(
            task_label(
                task_kind(&child),
                Some(&serde_json::json!({"description": "review"}))
            ),
            Some("review".to_string())
        );
        // The supported V2 state: a `subagent` launch whose metadata omitted
        // its session id. The neutral noun is the fact left; it must not read
        // as a shell row.
        let childless = task("subagent", None, None);
        assert_eq!(task_kind(&childless), TaskKind::Subagent);
        // A third adapter's identity-less name is not a subagent token: the
        // tolerant default stays shell.
        assert_eq!(task_kind(&task("spawn_agent", None, None)), TaskKind::Shell);
    }

    /// ADR-0118: a job-sourced task carries its job kind as the tool name, and
    /// each maps to its own ledger kind — never through the shell default. The
    /// job kind's token is also the word the row and the entry render, and only
    /// the kinds that show a total render an elapsed.
    #[test]
    fn a_jobs_kind_is_its_own_and_never_falls_through_to_shell() {
        let job = |name: &str| BackgroundTask {
            tool: ToolIdentity {
                name: name.into(),
                call_id: "job_1".into(),
            },
            job_id: Some("job_1".into()),
            ..Default::default()
        };
        for (name, kind) in [
            ("codemode", TaskKind::CodeMode),
            ("image", TaskKind::Image),
            ("video", TaskKind::Video),
            ("shell", TaskKind::Shell),
        ] {
            assert_eq!(task_kind(&job(name)), kind, "the `{name}` job's own kind");
            assert_eq!(kind.token(), name, "the kind word is the job's own token");
        }
        assert!(TaskKind::CodeMode.renders_elapsed());
        assert!(TaskKind::Image.renders_elapsed());
        assert!(TaskKind::Video.renders_elapsed());
        assert!(TaskKind::Shell.renders_elapsed());
        assert!(
            !TaskKind::Subagent.renders_elapsed(),
            "a subagent row's liveness is its activity fragment, never a total"
        );
    }

    /// ADR-0118: a job completion Wake's source marker names the job kind, and
    /// the notice stays a non-resumption (no 承接 line) — it is `Other`, never
    /// a genuine resumption or a completion.
    #[test]
    fn a_jobs_completion_wake_names_its_kind_and_stays_a_non_resumption() {
        let kind = |marker: &str| wake_task_kind(&WakeSource::Other(marker.into()));
        assert_eq!(kind("codemode"), Some(TaskKind::CodeMode));
        assert_eq!(kind("generate_image"), Some(TaskKind::Image));
        assert_eq!(kind("generate_video"), Some(TaskKind::Video));
        assert_eq!(kind("mystery"), None, "an unknown marker plans no entry");
        assert_eq!(wake_task_kind(&WakeSource::Restart), None);
        assert_eq!(wake_task_kind(&WakeSource::Interrupt), None);
        for marker in ["codemode", "generate_image", "generate_video"] {
            let source = WakeSource::Other(marker.into());
            assert!(!source.is_genuine_resumption(), "no 承接 line for `{marker}`");
            assert!(!source.is_completion(), "the notice is not a task handover");
        }
    }

    /// ADR-0118: the durable identity a job Wake's ending is recorded with
    /// names the job's kind token — so a later read re-synthesizes a job task,
    /// not a shell — and carries no invented launch call id.
    #[test]
    fn a_job_wakes_identity_names_its_kind() {
        let wake = Wake {
            id: MessageId::new("msg_wake_job"),
            created_ms: Some(2_000),
            source: WakeSource::Other("generate_image".into()),
            shell_id: None,
            job_id: Some("job_img".into()),
            child_id: None,
            state: Some("completed".into()),
            label: None,
        };
        let identity = identity_of_wake(&wake);
        assert_eq!(identity.tool.as_deref(), Some("image"));
        assert_eq!(identity.call_id, None, "a Wake never carried a call id");
        let task = identity.task("job_img");
        assert_eq!(task_kind(&task), TaskKind::Image);
        assert_eq!(task.tool.call_id, "job_img", "the run's key stands in");
    }

    /// ADR-0118: a **Child Job** marks its row with its owner — the owning
    /// subagent's label when the read resolved one, the neutral fallback when
    /// it did not — while an own-session task carries no marker at all. The
    /// owner label alone cannot decide it: both cases leave it `None`.
    #[test]
    fn only_a_child_job_carries_an_owner_marker() {
        let mut task = BackgroundTask {
            tool: ToolIdentity {
                name: "image".into(),
                call_id: "job_1".into(),
            },
            job_id: Some("job_1".into()),
            child_job: true,
            owner_label: Some("review the diff".into()),
            ..Default::default()
        };
        assert_eq!(
            child_job_owner(&task),
            Some(ChildJobOwner::Named("review the diff".into()))
        );
        task.owner_label = None;
        assert_eq!(
            child_job_owner(&task),
            Some(ChildJobOwner::Unnamed),
            "a child no row names still marks itself"
        );
        task.child_job = false;
        assert_eq!(
            child_job_owner(&task),
            None,
            "the session's own job carries no `↳`"
        );
    }

    /// A record carries EVERY id its run is known by: a later writer holding
    /// another spelling is folded in rather than minting a second record, and
    /// the stored key never moves.
    #[test]
    fn absorbing_a_later_writer_keeps_the_key_and_adds_the_ids() {
        let mut stored = RunKey::of_wake(&wake(Some("shell_1"), None, None)).expect("the wake names it");
        stored.absorb(&RunKey::of_launch(&launch_task("call_1", Some("shell_1"), None)));
        assert_eq!(
            stored.key, "shell_1",
            "the stored key is the run's own, not the newcomer's"
        );
        assert_eq!(stored.call_id.as_deref(), Some("call_1"));
        assert!(stored.names("call_1") && stored.names("shell_1") && stored.names("msg_wake_1"));
    }
}
