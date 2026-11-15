//! The Agent Run fact's derivation (ADR-0105, spec #1057's P4).
//!
//! A **child agent** is one run, whether the client calls it `task`,
//! `subagent`, `spawnAgent` or a Codex collaboration child. The adapter
//! declares the relation on each call ([`ToolDetail::Delegation`]): which runs
//! it names and whether it launches, addresses or reports on them. This module
//! folds those declarations, in transcript order, into the read's
//! [`AgentRun`]s. Nothing here reads a vendor id.
//!
//! The rules are spec #1057's:
//!
//! - **Two keys, one mapping.** `run_id` is the live identity — the child id —
//!   and never moves; `task_run_id` is ADR-0080's durable key and is the same
//!   child id, so every read window mints the same identity for one run. A
//!   fact naming no id names no run (both producers drop empty ids).
//! - **Multiplicity.** A launch naming no receiver creates no run; one naming
//!   N children creates N runs, each keyed by its own child.
//! - **A child may be launched more than once** — a resumed `task` — so
//!   `launched_by` is a set.
//! - **A report is an observation.** Identical observations collapse, and a
//!   repeated call id never appends a second relation (the wire reuses an
//!   activity's item id between its start and completion).
//! - **An addressing call may name no target** (`wait`, `listAgents`): it
//!   addresses the Turn's live runs as a set, so it mints nothing and attaches
//!   to no single run.

use super::{
    AgentRun, DelegationEffect, Part, Phase, RunActivity, RunStatus, SessionTranscript, ToolCall,
    ToolCallRef, ToolDetail, ToolStatus, TranscriptMessage,
};

/// Derive the runs one read names, in the order their first fact appears.
pub(crate) fn derive_runs(messages: &[TranscriptMessage]) -> Vec<AgentRun> {
    let mut runs: Vec<AgentRun> = Vec::new();
    for call in messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::Tool(call) => Some(call),
            _ => None,
        })
    {
        let ToolDetail::Delegation { runs: refs, effect } = &call.detail else {
            continue;
        };
        // A reference naming no id is not a run: an addressing call with no
        // target addresses the Turn's live runs as a SET and mints nothing
        // (spec #1057), and a producer strips the empty ids it cannot name. So
        // every ref this fold acts on names exactly one child. Its status is
        // the adapter's per-receiver declaration when it made one, else the
        // call's status (a one-run launch's own statement).
        let names: Vec<(&str, Option<RunStatus>)> = refs
            .iter()
            .filter(|reference| !reference.run_id.is_empty())
            .map(|reference| (reference.run_id.as_str(), reference.status))
            .collect();
        match effect {
            DelegationEffect::Launch => {
                for (run_id, receiver_status) in names {
                    let run = resolve_or_create(&mut runs, run_id, call, receiver_status);
                    push_relation(&mut run.launched_by, call);
                    // A resumed task names the same child: the run it resolves
                    // gains a launch, never a second record. The resume also
                    // moves the run's start when its clock is newer, so a
                    // pre-resume completion is stale evidence (the stale guard
                    // compares against this start).
                    if run.child_id.is_none() {
                        run.child_id = Some(run_id.to_string());
                    }
                    if let Some(at) = call.started_at
                        && run.started_at.is_none_or(|start| at > start)
                    {
                        run.started_at = Some(at);
                    }
                    // The launch call is the client's statement about the run
                    // it created or resumed: a resume is the newest statement
                    // when its clock is at or after the newest observation, so
                    // it makes the run live again. A multi-child launch may
                    // state each receiver's state separately; that declaration
                    // wins. A report newer than the launch is not overridden.
                    let launch_is_newest = call
                        .started_at
                        .is_none_or(|at| run.observed_at.is_none_or(|seen| at >= seen));
                    if launch_is_newest {
                        run.status = receiver_status.unwrap_or_else(|| status_of(call));
                        if let Some(at) = call.started_at {
                            run.observed_at = Some(at);
                        }
                    }
                }
            }
            DelegationEffect::Address => {
                for (run_id, receiver_status) in names {
                    let run = resolve_or_create(&mut runs, run_id, call, receiver_status);
                    push_relation(&mut run.addressed_by, call);
                }
            }
            DelegationEffect::Report => {
                for (run_id, receiver_status) in names {
                    let run = resolve_or_create(&mut runs, run_id, call, receiver_status);
                    // Dedupe by the CALL, never by call id alone: an activity
                    // whose state moved is the same call, so the relation is
                    // one entry and the state is applied below.
                    push_relation(&mut run.reported_by, call);
                    apply_report(run, call);
                }
            }
        }
    }
    runs
}

/// The run one delegation call names by `run_id`, created when no fact has
/// named it yet. The live identity is the child id (spec #1057), and a later
/// sighting resolves the same record by either its live identity or its child
/// id, so `run_id` never moves once minted.
///
/// `task_run_id` (ADR-0080's durable key) is the run's **child id**. A fact
/// names a child to create a run, and that id is the only key every read
/// window agrees on: a window starting at the launch, at a `wait`, or at a
/// report must mint the same durable identity, and a call-id key would differ
/// by which call the window happens to begin at (and could not identify two
/// runs named by one multi-child fact). No fact names a run without an id —
/// both producers drop empty ids — so no call-keyed arm exists.
fn resolve_or_create<'a>(
    runs: &'a mut Vec<AgentRun>,
    run_id: &str,
    call: &ToolCall,
    receiver_status: Option<RunStatus>,
) -> &'a mut AgentRun {
    if let Some(index) = runs
        .iter()
        .position(|run| run.run_id == run_id || run.child_id.as_deref() == Some(run_id))
    {
        return &mut runs[index];
    }
    runs.push(AgentRun {
        run_id: run_id.to_string(),
        task_run_id: Some(run_id.to_string()),
        child_id: Some(run_id.to_string()),
        label: label_of(call).unwrap_or_else(|| run_id.to_string()),
        agent: agent_of(call),
        started_at: call.started_at,
        status: receiver_status.unwrap_or_else(|| status_of(call)),
        activity: None,
        observed_at: None,
        launched_by: Vec::new(),
        addressed_by: Vec::new(),
        reported_by: Vec::new(),
    });
    runs.last_mut().expect("just pushed")
}

/// Add one relation unless that call is already recorded. The relation set is
/// keyed by call id, which is unique per call: a repeated report of the SAME
/// call is one relation whose state the run already carries.
fn push_relation(relations: &mut Vec<ToolCallRef>, call: &ToolCall) {
    if call.identity.call_id.is_empty() {
        return;
    }
    if relations
        .iter()
        .any(|reference| reference.call_id == call.identity.call_id)
    {
        return;
    }
    relations.push(ToolCallRef {
        call_id: call.identity.call_id.clone(),
    });
}

/// Apply one report observation. The state is the **adapter's declaration**
/// ([`ToolCall::status`]): a Codex `subAgentActivity` maps its wire `kind` onto
/// that status before it reaches the read model, so this fold never parses a
/// vendor payload. A state a later report supersedes wins because the fold is
/// in transcript order; the call id is never the dedupe key, because the wire
/// reuses an activity item's id between its start and its completion.
fn apply_report(run: &mut AgentRun, call: &ToolCall) {
    // A replayed observation older than the newest one is history: the relation
    // is already recorded by the caller, but it must never move the status
    // back — a completed child cannot read live again because an old
    // `started` arrived late (the wire reuses an activity item's id, and a
    // replay can re-deliver it).
    if let (Some(seen), Some(reported_at)) = (run.observed_at, call.started_at)
        && reported_at < seen
    {
        return;
    }
    if call.started_at.is_some() {
        run.observed_at = call.started_at;
    }
    let status = status_of(call);
    run.status = status;
    if status.is_terminal() {
        // The client states the run ended: no live activity to render. The
        // durable ending still comes from the child read (the ledger's entry
        // and ADR-0080 record), never from this observation.
        run.activity = None;
        return;
    }
    let activity = run.activity.get_or_insert(RunActivity {
        call: None,
        phase: None,
        awaiting: None,
        since: call.started_at,
    });
    if activity.phase.is_none() && activity.call.is_none() {
        // The report says the child is live, not what it is doing: `Waiting`
        // is the honest phase until the child's own read establishes one.
        activity.phase = Some(Phase::Waiting);
    }
    if activity.since.is_none() {
        activity.since = call.started_at;
    }
}

/// The client's status for the call that created or resumed the run. A launch
/// call's `Completed` means the client's statement is "this run is done" — a
/// foreground `task` returns only when its child ends, and a resumed task that
/// settled is a finished child. A client whose launch is only a handle states
/// the child's later state through a `subAgentActivity` report.
fn status_of(call: &ToolCall) -> RunStatus {
    match &call.status {
        ToolStatus::Error => RunStatus::Failed,
        ToolStatus::Other(reason) if reason == "interrupted" => RunStatus::Cancelled,
        ToolStatus::Completed => RunStatus::Completed,
        ToolStatus::Running | ToolStatus::Pending => RunStatus::Running,
        ToolStatus::Other(_) | ToolStatus::Unknown => RunStatus::Unknown,
    }
}

/// The readable label the client gave the child: an explicit label, a
/// description, the prompt's first non-empty line, or the run id (the caller's
/// fallback). Never invented beyond the payload's own words.
fn label_of(call: &ToolCall) -> Option<String> {
    if let Some(title) = call.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        return Some(title.to_string());
    }
    let input = call.input.as_ref()?;
    for key in ["label", "description", "agentPath", "prompt"] {
        let Some(text) = input.get(key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        // A field that is present but blank is not a label: keep probing the
        // remaining fields rather than abandoning the whole payload.
        let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
            continue;
        };
        return Some(line.to_string());
    }
    None
}

/// The subagent type, agent path or model the call named.
fn agent_of(call: &ToolCall) -> Option<String> {
    let input = call.input.as_ref()?;
    for key in ["subagent_type", "agent", "agentPath", "model"] {
        if let Some(text) = input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(text.to_string());
        }
    }
    None
}

impl SessionTranscript {
    /// The runs' child ids still live, for the render's shared liveness gather:
    /// one `(run_id, child_id)` per live run that named a child. A run with no
    /// child id renders its label and status (the acceptance rule) but has
    /// nothing to read.
    pub fn run_children(&self) -> Vec<(String, String)> {
        self.runs
            .iter()
            .filter(|run| run.is_live())
            .filter_map(|run| run.child_id.clone().map(|child| (run.run_id.clone(), child)))
            .collect()
    }
}

/// One child transcript's live activity, in neutral semantics (ADR-0054). The
/// newest live tool is the call; otherwise the newest part implies the phase.
/// `None` when the read carries no timestamp at all — an untimed read proves no
/// clock, and the card must not invent one. This is the same evidence the
/// OpenCode child-liveness gather has always used, now stated as the neutral
/// [`RunActivity`] rather than a card-private type.
pub fn child_run_activity(transcript: &SessionTranscript) -> Option<RunActivity> {
    let mut newest_ms: Option<i64> = None;
    let mut current: Option<(i64, &ToolCall)> = None;
    let mut newest_part: Option<&Part> = None;
    let observe = |newest: &mut Option<i64>, at: Option<i64>| {
        if let Some(at) = at {
            *newest = Some(newest.map_or(at, |held| held.max(at)));
        }
    };
    for message in &transcript.messages {
        if let Some(time) = &message.time {
            observe(&mut newest_ms, Some(time.completed.unwrap_or(time.created)));
        }
        for part in &message.parts {
            newest_part = Some(part);
            match part {
                Part::Tool(call) => {
                    observe(&mut newest_ms, call.started_at);
                    if call.status.is_live() {
                        let at = call.started_at.unwrap_or(i64::MIN);
                        if current.as_ref().is_none_or(|(best, _)| at >= *best) {
                            current = Some((at, call));
                        }
                    }
                }
                Part::Text(_) | Part::Reasoning(_) => observe(&mut newest_ms, part.started_at()),
                Part::StepStart(_)
                | Part::StepFinish(_)
                | Part::Patch(_)
                | Part::File(_)
                | Part::Compaction(_)
                | Part::Other(_) => {}
            }
        }
    }
    let call = current.map(|(_, call)| super::CallView {
        identity: call.identity.clone(),
        kind: call.kind,
        detail: call.detail.clone(),
        started_at: call.started_at,
    });
    let phase = if call.is_some() {
        None
    } else {
        match newest_part {
            Some(Part::Reasoning(_)) => Some(Phase::Reasoning),
            Some(Part::Text(_)) => Some(Phase::Responding),
            _ => Some(Phase::Waiting),
        }
    };
    let since = newest_ms?;
    Some(RunActivity {
        call,
        phase,
        awaiting: None,
        since: Some(since),
    })
}

/// The newest step finish a child transcript's newest assistant message
/// carries, if any. The one read both the client's status statement and the
/// liveness suppression use, so they cannot disagree about which finish is
/// "the finish".
fn newest_step_finish(transcript: &SessionTranscript) -> Option<&super::FinishReason> {
    let message = transcript
        .messages
        .iter()
        .rev()
        .find(|message| message.role == super::MessageRole::Assistant)?;
    message.parts.iter().rev().find_map(|part| match part {
        Part::StepFinish(finish) => Some(&finish.reason),
        _ => None,
    })
}

/// The client's terminal statement a child transcript carries, if any: its
/// newest step finish is a terminal reason, with the message's completion
/// stamp. This is the run's `RunStatus` (the client's own statement) — never
/// cola's `TaskEnding`, which the evidence path records separately.
pub fn child_run_status(transcript: &SessionTranscript) -> Option<RunStatus> {
    match newest_step_finish(transcript)? {
        super::FinishReason::Error => Some(RunStatus::Failed),
        super::FinishReason::Stop | super::FinishReason::Length | super::FinishReason::ContentFilter => {
            Some(RunStatus::Completed)
        }
        // A reason this build cannot name is not a client statement that the
        // run was cancelled: it still owes an exit (its evidence or a
        // cleanup), the same honesty `ToolStatus::Other` gets in `status_of`.
        super::FinishReason::Other(_) => Some(RunStatus::Unknown),
        super::FinishReason::ToolCalls | super::FinishReason::Unknown => None,
    }
}

/// Whether the child has finished, whatever [`RunStatus`] its finish maps to.
///
/// The status word is terminal for the ordinary finishes; a terminal finish
/// that maps to a non-terminal status is the case the status alone misses:
/// `Other` is a **terminal** reason — Codex's `interrupted`, OpenCode's
/// spelled-out `unknown` — yet maps to `RunStatus::Unknown` so the client's
/// statement stays honest. A finished child must not keep rendering a
/// live-looking `waiting <age>` line while the run awaits its evidence, so
/// liveness asks this, not the status name.
pub fn child_run_finished(transcript: &SessionTranscript) -> bool {
    // Either statement ends the liveness line: the status a terminal finish
    // maps to, and the terminal finish itself (which `Other` keeps out of the
    // status so the client's statement stays honest).
    child_run_status(transcript).is_some_and(|status| status.is_terminal())
        || newest_step_finish(transcript).is_some_and(super::FinishReason::is_terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        AgentRunRef, ChildEvidence, FinishReason, MessageId, MessageTime, StepFinish, ToolIdentity,
    };

    fn call(
        call_id: &str,
        effect: DelegationEffect,
        runs: &[&str],
        status: ToolStatus,
        started_at: Option<i64>,
    ) -> ToolCall {
        ToolCall {
            identity: ToolIdentity {
                name: "task".to_string(),
                call_id: call_id.to_string(),
            },
            status,
            started_at,
            input: Some(serde_json::json!({"description": format!("run {call_id}")})),
            detail: ToolDetail::Delegation {
                runs: runs
                    .iter()
                    .map(|id| AgentRunRef {
                        run_id: id.to_string(),
                        status: None,
                    })
                    .collect(),
                effect,
            },
            ..ToolCall::default()
        }
    }

    /// One report call the way the Codex adapter declares it: the wire
    /// `kind` already mapped onto the call's status, so the fold reads the
    /// adapter's declaration and never the raw payload.
    fn report(call_id: &str, child: &str, kind: &str) -> ToolCall {
        let status = match kind {
            "completed" => ToolStatus::Completed,
            "interrupted" => ToolStatus::Error,
            _ => ToolStatus::Running,
        };
        let mut call = call(call_id, DelegationEffect::Report, &[child], status, Some(2));
        call.input = Some(serde_json::json!({"kind": kind, "agentThreadId": child}));
        call
    }

    fn transcript(calls: Vec<ToolCall>) -> SessionTranscript {
        SessionTranscript::new(vec![TranscriptMessage {
            id: MessageId::new("msg_1"),
            role: super::super::MessageRole::Assistant,
            time: Some(MessageTime {
                created: 1,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: calls.into_iter().map(Part::Tool).collect(),
        }])
    }

    #[test]
    fn a_launch_naming_a_receiver_creates_one_run() {
        let read = transcript(vec![call(
            "call_task",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(10),
        )]);
        assert_eq!(read.runs.len(), 1);
        let run = &read.runs[0];
        assert_eq!(run.run_id, "ses_child", "the live identity is the child id");
        assert_eq!(
            run.task_run_id.as_deref(),
            Some("ses_child"),
            "ADR-0080's key is the child id every window agrees on"
        );
        assert_eq!(run.child_id.as_deref(), Some("ses_child"));
        assert_eq!(run.label, "run call_task", "the client's description labels it");
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.launched_by[0].call_id, "call_task");
    }

    #[test]
    fn a_launch_naming_no_receiver_creates_no_run() {
        let read = transcript(vec![call(
            "call_task",
            DelegationEffect::Launch,
            &[],
            ToolStatus::Running,
            Some(10),
        )]);
        assert!(read.runs.is_empty(), "a receiverless launch mints nothing");
    }

    /// An addressing call with no target addresses the Turn's runs as a SET:
    /// it mints nothing (spec #1057). The same holds for a report naming no
    /// child — an empty reference is not a run identity, and the producers
    /// strip the ids they cannot name.
    #[test]
    fn an_unnamed_reference_mints_nothing() {
        let mut wait = call(
            "call_wait",
            DelegationEffect::Address,
            &[],
            ToolStatus::Completed,
            Some(5),
        );
        wait.input = Some(serde_json::json!({"tool": "wait"}));
        let mut empty_address = call(
            "call_empty",
            DelegationEffect::Address,
            &[""],
            ToolStatus::Completed,
            Some(5),
        );
        empty_address.input = Some(serde_json::json!({"tool": "sendInput"}));
        let mut empty_report = call(
            "call_activity",
            DelegationEffect::Report,
            &[""],
            ToolStatus::Running,
            Some(6),
        );
        empty_report.input = Some(serde_json::json!({"kind": "started", "agentThreadId": ""}));
        let read = transcript(vec![wait, empty_address, empty_report]);
        assert!(
            read.runs.is_empty(),
            "a call that names no run mints no run: {:?}",
            read.runs
        );
        assert!(read.run_evidence_suspects().is_empty());
    }

    /// A report naming the child before its launch creates the run, and the
    /// later launch resolves it: one record, and `run_id` never moves.
    #[test]
    fn a_report_before_its_launch_resolves_the_same_run() {
        let read = transcript(vec![
            report("call_activity", "thread_1", "started"),
            call(
                "call_spawn",
                DelegationEffect::Launch,
                &["thread_1"],
                ToolStatus::Running,
                Some(3),
            ),
        ]);
        assert_eq!(read.runs.len(), 1, "one child is one run");
        let run = &read.runs[0];
        assert_eq!(run.run_id, "thread_1");
        assert_eq!(
            run.task_run_id.as_deref(),
            Some("thread_1"),
            "the first id-bearing fact mints the durable key, never re-minted"
        );
        assert_eq!(run.launched_by.len(), 1, "the later launch is a relation");
        assert_eq!(run.reported_by.len(), 1);
    }

    /// Spec #1057's acceptance: a run with no child id (if a producer has none)
    /// still carries its label and status, and its identity/durable key come
    /// from the creating call — never an empty-id record. No producer emits one
    /// today (both strip ids they cannot name), so the shape is asserted
    /// directly.
    #[test]
    fn a_run_with_no_child_id_keeps_its_label_and_key() {
        let run = AgentRun {
            run_id: "call_creator".into(),
            task_run_id: Some("call_creator".into()),
            child_id: None,
            label: "review the diff".into(),
            agent: Some("worker".into()),
            started_at: Some(1),
            status: RunStatus::Running,
            activity: None,
            observed_at: None,
            launched_by: Vec::new(),
            addressed_by: Vec::new(),
            reported_by: Vec::new(),
        };
        let mut read = SessionTranscript::default();
        read.runs = vec![run];
        assert_eq!(read.runs[0].ending_key(), "call_creator");
        assert!(read.runs[0].is_live());
        assert!(
            read.run_children().is_empty(),
            "nothing to read for a run with no child id"
        );
        assert!(read.run_evidence_suspects().is_empty());
    }

    /// A launch naming N children mints a DISTINCT durable key per run
    /// (spec #1057's P4, review finding 1): one call id cannot identify two
    /// runs' endings. A single-receiver fact keeps the call id (ADR-0080).
    #[test]
    fn one_launch_naming_two_children_mints_one_key_each() {
        let read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["thread_a", "thread_b"],
            ToolStatus::Running,
            Some(10),
        )]);
        assert_eq!(read.runs.len(), 2);
        let keys: Vec<&str> = read.runs.iter().map(AgentRun::ending_key).collect();
        assert_eq!(keys, ["thread_a", "thread_b"]);
        assert_ne!(keys[0], keys[1], "two runs never share a durable key");
        assert!(
            read.runs
                .iter()
                .all(|run| run.task_run_id.as_deref() == run.child_id.as_deref()),
            "a multi-receiver fact keys each run by its own child id"
        );
    }

    /// A multi-child launch may state each receiver's own status. The per-ref
    /// declaration wins over the call's single status: one completed child must
    /// not read live because a sibling is still running (spec #1057's
    /// `agentsStates`).
    #[test]
    fn a_multi_child_launch_keeps_each_receivers_own_status() {
        let mut spawn = call(
            "call_spawn",
            DelegationEffect::Launch,
            &[],
            ToolStatus::Running,
            Some(10),
        );
        spawn.detail = ToolDetail::Delegation {
            runs: vec![
                AgentRunRef {
                    run_id: "thread_a".into(),
                    status: Some(RunStatus::Completed),
                },
                AgentRunRef {
                    run_id: "thread_b".into(),
                    status: Some(RunStatus::Running),
                },
            ],
            effect: DelegationEffect::Launch,
        };
        let read = transcript(vec![spawn]);
        let a = read
            .runs
            .iter()
            .find(|run| run.run_id == "thread_a")
            .expect("run A");
        let b = read
            .runs
            .iter()
            .find(|run| run.run_id == "thread_b")
            .expect("run B");
        assert_eq!(a.status, RunStatus::Completed);
        assert_eq!(b.status, RunStatus::Running);
        assert!(!a.is_live(), "the completed child is not live");
        assert!(b.is_live(), "the running child stays live");
    }

    /// A report's state is the adapter's declaration: a completed observation
    /// ends the run, an interrupted one fails it, and an unknown/new activity
    /// kind (declared `Running`) keeps it live — the fold never parses the
    /// vendor's `kind`.
    #[test]
    fn a_report_uses_the_adapters_status() {
        let read = transcript(vec![
            report("call_start", "thread_1", "started"),
            report("call_done", "thread_1", "completed"),
        ]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
        assert_eq!(read.runs[0].activity, None, "no live activity once ended");

        let read = transcript(vec![report("call_interrupt", "thread_2", "interrupted")]);
        assert_eq!(read.runs[0].status, RunStatus::Failed);

        let mut unknown = report("call_unknown", "thread_3", "started");
        unknown.status = ToolStatus::Running;
        let read = transcript(vec![unknown]);
        assert_eq!(read.runs[0].status, RunStatus::Running);
    }

    /// A replayed older observation must not revive a run a later report
    /// already ended: the relation stays recorded, the status does not move
    /// back.
    #[test]
    fn an_older_report_cannot_revive_a_terminal_run() {
        let mut completed = report("call_done", "thread_1", "completed");
        completed.started_at = Some(3);
        let mut replayed = report("call_started_old", "thread_1", "started");
        replayed.started_at = Some(2);
        let read = transcript(vec![completed, replayed]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
        assert!(
            read.runs[0].activity.is_none(),
            "a terminal run keeps no live activity"
        );
        assert_eq!(
            read.runs[0].reported_by.len(),
            2,
            "the older observation is still a relation on the run"
        );
    }

    /// The durable key is a property of the RUN, not of where a read window
    /// starts: a launch first and a `wait` first name the same child and must
    /// mint the same key, or the ledger would key one run's endings two ways.
    #[test]
    fn every_read_window_mints_the_same_durable_key() {
        let launch = || {
            call(
                "call_spawn",
                DelegationEffect::Launch,
                &["thread_1"],
                ToolStatus::Running,
                Some(1),
            )
        };
        let wait = || {
            call(
                "call_wait",
                DelegationEffect::Address,
                &["thread_1"],
                ToolStatus::Completed,
                Some(2),
            )
        };
        let full = transcript(vec![launch(), wait()]);
        let late = transcript(vec![wait()]);
        let reported = transcript(vec![report("call_activity", "thread_1", "started")]);
        assert_eq!(full.runs[0].ending_key(), "thread_1");
        assert_eq!(
            full.runs[0].ending_key(),
            late.runs[0].ending_key(),
            "a window starting at the addressing call keys the same run"
        );
        assert_eq!(
            full.runs[0].ending_key(),
            reported.runs[0].ending_key(),
            "a window starting at a report keys the same run"
        );
    }

    /// A resumed `task` launches the same child again: `launched_by` is a set,
    /// and no second run appears.
    #[test]
    fn a_resumed_task_adds_a_launch_to_the_same_run() {
        let read = transcript(vec![
            call(
                "call_1",
                DelegationEffect::Launch,
                &["ses_child"],
                ToolStatus::Running,
                Some(1),
            ),
            call(
                "call_2",
                DelegationEffect::Launch,
                &["ses_child"],
                ToolStatus::Running,
                Some(2),
            ),
        ]);
        assert_eq!(read.runs.len(), 1);
        let calls: Vec<&str> = read.runs[0]
            .launched_by
            .iter()
            .map(|reference| reference.call_id.as_str())
            .collect();
        assert_eq!(calls, ["call_1", "call_2"]);
        assert_eq!(
            read.runs[0].task_run_id.as_deref(),
            Some("ses_child"),
            "the key is minted once"
        );
    }

    /// A resume is the newest client statement: it moves the run's start and
    /// makes it live again, so a completion before the resume is stale evidence
    /// and cannot retire the freshly resumed run.
    #[test]
    fn a_resumed_launch_refreshes_the_run_and_rejects_pre_resume_evidence() {
        let mut completed = report("call_activity", "ses_child", "completed");
        completed.started_at = Some(4);
        let mut read = transcript(vec![
            call(
                "call_1",
                DelegationEffect::Launch,
                &["ses_child"],
                ToolStatus::Running,
                Some(1),
            ),
            completed,
            call(
                "call_2",
                DelegationEffect::Launch,
                &["ses_child"],
                ToolStatus::Running,
                Some(5),
            ),
        ]);
        let run = &read.runs[0];
        assert_eq!(run.started_at, Some(5), "the resume moves the run's start");
        assert_eq!(
            run.status,
            RunStatus::Running,
            "the resume is the newest client statement"
        );
        assert!(run.is_live());

        assert!(
            !read.apply_run_child_evidence("ses_child", ChildEvidence::Terminal { completed_at: 4 }),
            "a pre-resume completion is older than the resumed start"
        );
        assert_eq!(read.runs.len(), 1, "the resumed run stays live");
    }

    /// A report is an observation applied by its state: the same call's start
    /// and completion both apply (the wire reuses the item id), and the relation
    /// stays one entry.
    #[test]
    fn one_activity_item_updates_rather_than_appends() {
        let started = report("call_activity", "thread_1", "started");
        let completed = report("call_activity", "thread_1", "completed");
        let read = transcript(vec![started, completed]);
        assert_eq!(read.runs.len(), 1);
        let run = &read.runs[0];
        assert_eq!(run.status, RunStatus::Completed, "the completion applies");
        assert_eq!(run.reported_by.len(), 1, "one call is one relation");
    }

    /// An addressing call with a target is a relation on that run, and only a
    /// launch keeps a panel (the surface rule lives above this fact).
    #[test]
    fn addressing_calls_attach_to_their_run_and_mint_no_second_one() {
        let read = transcript(vec![
            call(
                "call_spawn",
                DelegationEffect::Launch,
                &["thread_1"],
                ToolStatus::Running,
                Some(1),
            ),
            call(
                "call_input",
                DelegationEffect::Address,
                &["thread_1"],
                ToolStatus::Completed,
                Some(2),
            ),
        ]);
        assert_eq!(read.runs.len(), 1);
        assert_eq!(read.runs[0].addressed_by[0].call_id, "call_input");
    }

    /// A child approval still resolves through the parent chain: the run fact
    /// does not change the request path, so the run's child id is the same id
    /// the request card re-homes by.
    #[test]
    fn a_childs_run_names_the_child_its_request_rehomes_by() {
        let read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(1),
        )]);
        assert_eq!(
            read.run_children(),
            vec![("ses_child".to_string(), "ses_child".to_string())]
        );
    }

    /// Review finding 6: a run a report already declared terminal is still an
    /// evidence suspect. The client's statement removes the live row, but only
    /// the child's own read supplies the durable `结束` ending — otherwise the
    /// row would vanish with no entry and no ADR-0080 record.
    #[test]
    fn a_report_terminal_run_remains_an_evidence_suspect() {
        let read = transcript(vec![report("call_done", "thread_1", "completed")]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
        assert!(!read.runs[0].is_live(), "the client states the run ended");
        assert_eq!(
            read.run_evidence_suspects(),
            vec![("thread_1".to_string(), "thread_1".to_string())],
            "the child read still has to confirm the end"
        );

        let mut read = read;
        assert!(read.apply_run_child_evidence("thread_1", ChildEvidence::Terminal { completed_at: 3 }));
        assert_eq!(read.task_retirements.len(), 1);
        assert_eq!(
            read.task_retirements[0].label.as_deref(),
            Some("thread_1"),
            "the entry names the run (its own label fallback here)"
        );
    }

    /// A run a Background Task already represents is not read twice: the
    /// runtime's child path owns it, and the two rules share one predicate.
    #[test]
    fn a_run_represented_by_a_task_is_not_an_evidence_suspect() {
        let mut read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["thread_1"],
            ToolStatus::Running,
            Some(1),
        )]);
        read.background_tasks.push(crate::backend::BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_spawn".into(),
            },
            shell_id: None,
            child_id: Some("thread_1".into()),
            started_at: Some(1),
        });
        assert!(read.run_evidence_suspects().is_empty());
        assert!(read.run_is_task(&read.runs[0], &read.background_tasks[0]));
    }

    /// A child that ends with no Wake retires through its child evidence: the
    /// retirement carries ADR-0080's identity, not a client status.
    #[test]
    fn a_childs_terminal_evidence_retires_its_run() {
        let mut read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(1),
        )]);
        assert!(read.apply_run_child_evidence("ses_child", ChildEvidence::Terminal { completed_at: 9 }));
        assert!(read.runs.is_empty(), "the run leaves the live list");
        assert_eq!(read.task_retirements.len(), 1);
        let retirement = &read.task_retirements[0];
        assert_eq!(retirement.ending, super::super::TaskRetirementEnding::ChildEnded);
        assert_eq!(retirement.finished_at, Some(9));
        assert_eq!(
            retirement.task.tool.call_id, "ses_child",
            "the retirement names the run by its durable key"
        );
        assert_eq!(
            retirement.task.tool.name, "subagent",
            "the ledger's row stays subagent"
        );
    }

    /// Codex's `completedAt` is epoch seconds on the wire and milliseconds at
    /// the adapter boundary. A millisecond completion after the run's start
    /// retires it; a raw seconds value would look older than the start and be
    /// rejected as a previous turn's evidence.
    #[test]
    fn a_millisecond_completion_after_the_run_start_retires_it() {
        let mut read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(1_700_000_000_000),
        )]);
        assert!(read.apply_run_child_evidence(
            "ses_child",
            ChildEvidence::Terminal {
                completed_at: 1_700_000_002_000
            }
        ));
        assert!(read.runs.is_empty(), "the run retires on its child's evidence");
        assert_eq!(read.task_retirements[0].finished_at, Some(1_700_000_002_000));
    }

    /// A client's terminal statement is mapped honestly: a reason this build
    /// cannot name is `Unknown` — not `Cancelled`, which would put words in
    /// the client's mouth and let the run look concluded. `Unknown` is not
    /// terminal, so the run still owes its exit (its evidence or a cleanup),
    /// exactly as [`RunStatus::is_terminal`] documents.
    #[test]
    fn an_unknown_finish_reason_is_unknown_and_still_owes_an_exit() {
        let finished = |reason: FinishReason| {
            SessionTranscript::new(vec![TranscriptMessage {
                id: MessageId::new("msg_finish"),
                role: super::super::MessageRole::Assistant,
                time: Some(MessageTime {
                    created: 1,
                    completed: Some(2),
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::StepFinish(StepFinish { reason })],
            }])
        };

        let unknown = child_run_status(&finished(FinishReason::Other("vendor-x".into())));
        assert_eq!(
            unknown,
            Some(RunStatus::Unknown),
            "an unnameable finish reason is an unknown statement, not a cancellation"
        );
        assert!(
            !unknown.expect("a statement").is_terminal(),
            "an unknown statement still owes an exit"
        );
        assert_eq!(child_run_status(&finished(FinishReason::Unknown)), None);
        assert_eq!(child_run_status(&finished(FinishReason::ToolCalls)), None);
        assert_eq!(
            child_run_status(&finished(FinishReason::Error)),
            Some(RunStatus::Failed)
        );
        assert_eq!(
            child_run_status(&finished(FinishReason::Stop)),
            Some(RunStatus::Completed)
        );
    }
}
