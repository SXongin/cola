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
//! - **Two keys, one mapping.** `run_id` is the live identity (the child id
//!   when one exists, else the creating call's id) and never moves;
//!   `task_run_id` is ADR-0080's durable key, minted by the first id-bearing
//!   fact and never re-minted.
//! - **Multiplicity.** A launch naming no receiver creates no run. A run
//!   created from a call-only fact fills `child_id` later and keeps its
//!   `run_id`.
//! - **A child may be launched more than once** — a resumed `task` — so
//!   `launched_by` is a set.
//! - **A report is an observation.** Identical observations collapse, and a
//!   repeated call id never appends a second relation (the wire reuses an
//!   activity's item id between its start and completion).
//! - **An addressing call may name no target** (`wait`, `listAgents`): it
//!   addresses the Turn's live runs as a set, so it mints nothing and attaches
//!   to no single run.

use super::{
    AgentRun, AgentRunRef, DelegationEffect, Part, Phase, RunActivity, RunStatus, SessionTranscript,
    ToolCall, ToolCallRef, ToolDetail, ToolStatus, TranscriptMessage,
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
        match effect {
            DelegationEffect::Launch => {
                for reference in refs {
                    if reference.run_id.is_empty() {
                        continue;
                    }
                    let run = resolve_or_create(&mut runs, reference, call);
                    push_relation(&mut run.launched_by, call);
                    // A resumed task names the same child: the run it resolves
                    // gains a launch, never a second record.
                    if run.child_id.is_none() {
                        run.child_id = Some(reference.run_id.clone());
                    }
                    // The launch call is the client's statement about the run
                    // it created or resumed: a foreground `task` settles only
                    // when its child ends, so its own status is the child's.
                    // A client whose launch is only a handle states the
                    // child's later state through a report, and a report is
                    // the newer statement — it is never overridden here.
                    if run.reported_by.is_empty() {
                        run.status = status_of(call);
                    }
                }
            }
            DelegationEffect::Address => {
                // No receiver: a turn-level receipt. It names the Turn's live
                // runs as a set and mints nothing (spec #1057). A reference
                // that names no id is a call-keyed run — a non-launch fact
                // without a child id, which fills it on a later sighting.
                for reference in refs {
                    let run = resolve_or_create(&mut runs, reference, call);
                    fill_child(run, reference);
                    push_relation(&mut run.addressed_by, call);
                }
            }
            DelegationEffect::Report => {
                for reference in refs {
                    let run = resolve_or_create(&mut runs, reference, call);
                    fill_child(run, reference);
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

/// The run a delegation call names, created when no fact has named it yet.
///
/// - A reference naming an id is keyed by it (the child id — the live
///   identity).
/// - A reference naming **no** id is a non-launch fact without a child id: the
///   run is keyed by the creating call's id, so no two such runs can share an
///   empty-id identity, and a later sighting of the same call fills
///   `child_id`.
/// - A later sighting of the same call (its `task_run_id`) resolves the run it
///   created even when that fact now names the child id, so `run_id` never
///   moves once minted.
fn resolve_or_create<'a>(
    runs: &'a mut Vec<AgentRun>,
    reference: &AgentRunRef,
    call: &ToolCall,
) -> &'a mut AgentRun {
    let named = !reference.run_id.is_empty();
    if let Some(index) = runs.iter().position(|run| {
        (named
            && (run.run_id == reference.run_id || run.child_id.as_deref() == Some(reference.run_id.as_str())))
            || (run.child_id.is_none() && run.task_run_id.as_deref() == Some(call.identity.call_id.as_str()))
    }) {
        return &mut runs[index];
    }
    // `task_run_id` is minted from the creating call (ADR-0080) — never
    // invented when the call has no id. An unnamed reference takes the call's
    // id as the live identity, which is the only id the fact carries.
    let task_run_id = (!call.identity.call_id.is_empty()).then(|| call.identity.call_id.clone());
    let run_id = if named {
        reference.run_id.clone()
    } else {
        call.identity.call_id.clone()
    };
    runs.push(AgentRun {
        run_id: run_id.clone(),
        task_run_id,
        child_id: named.then(|| reference.run_id.clone()),
        label: label_of(call).unwrap_or_else(|| run_id.clone()),
        agent: agent_of(call),
        started_at: call.started_at,
        status: status_of(call),
        activity: None,
        launched_by: Vec::new(),
        addressed_by: Vec::new(),
        reported_by: Vec::new(),
    });
    runs.last_mut().expect("just pushed")
}

/// Fill a call-keyed run's child id from a later sighting without moving
/// `run_id` (spec #1057: the live identity is minted once; only `child_id`
/// fills).
fn fill_child(run: &mut AgentRun, reference: &AgentRunRef) {
    if run.child_id.is_none() && !reference.run_id.is_empty() {
        run.child_id = Some(reference.run_id.clone());
    }
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

/// Apply one `subAgentActivity` observation. The activity's `kind` is the state
/// the client reported; identical states collapse, and a state a later report
/// supersedes wins because the fold is in transcript order. The call id is never
/// the dedupe key: the wire reuses an activity item's id between its start and
/// its completion, and the completion must still apply.
fn apply_report(run: &mut AgentRun, call: &ToolCall) {
    let kind = call
        .input
        .as_ref()
        .and_then(|input| input.get("kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let (status, phase) = match kind {
        "started" | "interacted" | "resumed" => (RunStatus::Running, Some(Phase::Reasoning)),
        "completed" => (RunStatus::Completed, None),
        "interrupted" => (RunStatus::Cancelled, None),
        // An activity kind this build does not know: the client reported
        // something, but not a status this read can name — never guessed.
        _ => (RunStatus::Running, None),
    };
    if run.status != status {
        run.status = status;
    }
    if let Some(phase) = phase {
        let activity = run.activity.get_or_insert(RunActivity {
            call: None,
            phase: None,
            awaiting: None,
            since: call.started_at,
        });
        if activity.phase.is_none() && activity.call.is_none() {
            activity.phase = Some(phase);
        }
        if activity.since.is_none() {
            activity.since = call.started_at;
        }
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
        let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
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

/// The client's terminal statement a child transcript carries, if any: its
/// newest step finish is a terminal reason, with the message's completion
/// stamp. This is the run's `RunStatus` (the client's own statement) — never
/// cola's `TaskEnding`, which the evidence path records separately.
pub fn child_run_status(transcript: &SessionTranscript) -> Option<RunStatus> {
    let message = transcript
        .messages
        .iter()
        .rev()
        .find(|message| message.role == super::MessageRole::Assistant)?;
    message.parts.iter().rev().find_map(|part| match part {
        Part::StepFinish(finish) => match &finish.reason {
            super::FinishReason::Error => Some(RunStatus::Failed),
            super::FinishReason::Stop | super::FinishReason::Length | super::FinishReason::ContentFilter => {
                Some(RunStatus::Completed)
            }
            super::FinishReason::Other(_) => Some(RunStatus::Cancelled),
            super::FinishReason::ToolCalls | super::FinishReason::Unknown => None,
        },
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ChildEvidence, MessageId, MessageTime, ToolIdentity};

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
                    })
                    .collect(),
                effect,
            },
            ..ToolCall::default()
        }
    }

    fn report(call_id: &str, child: &str, kind: &str) -> ToolCall {
        let mut call = call(
            call_id,
            DelegationEffect::Report,
            &[child],
            ToolStatus::Completed,
            Some(2),
        );
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
        assert_eq!(run.task_run_id.as_deref(), Some("call_task"), "ADR-0080's key");
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

    /// The identity is minted once: a call-keyed run keeps its `run_id` when a
    /// later fact names its child id, and only `child_id` fills in.
    #[test]
    fn a_call_keyed_run_never_rekeys_when_its_child_id_appears() {
        // `wait` addresses a run it can only name by its own call (the shape a
        // wire gives when the target list is empty): the run is keyed by the
        // creating call's id.
        let mut address = call(
            "call_wait",
            DelegationEffect::Address,
            &[],
            ToolStatus::Running,
            Some(5),
        );
        address.input = Some(serde_json::json!({"tool": "wait"}));
        let read = transcript(vec![address]);
        assert!(
            read.runs.is_empty(),
            "an addressing call with no target mints nothing"
        );

        // A report naming the child before its launch creates the run keyed by
        // the child, which the later launch resolves (never a second record).
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
            Some("call_activity"),
            "the first id-bearing fact mints the durable key, never re-minted"
        );
        assert_eq!(run.launched_by.len(), 1, "the later launch is a relation");
        assert_eq!(run.reported_by.len(), 1);
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
            Some("call_1"),
            "the key is minted once"
        );
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

    /// A non-launch fact that names no child id creates a CALL-KEYED run: the
    /// creating call's id is the live identity, `task_run_id` is ADR-0080's
    /// key, and nothing shares an empty-id identity. A later sighting of the
    /// same call fills `child_id` and never moves the key.
    #[test]
    fn a_call_keyed_run_keeps_its_key_and_fills_a_later_child_id() {
        let mut anonymous = report("call_report", "", "started");
        anonymous.input = Some(serde_json::json!({"kind": "started", "agentThreadId": ""}));
        let read = transcript(vec![anonymous.clone()]);
        assert_eq!(read.runs.len(), 1);
        let run = &read.runs[0];
        assert_eq!(
            run.run_id, "call_report",
            "the creating call's id, never an empty one"
        );
        assert_eq!(run.child_id, None, "no child id was reported yet");
        assert_eq!(run.task_run_id.as_deref(), Some("call_report"));
        assert!(!run.label.is_empty(), "the run still renders a label");

        // The same activity's completion (one wire item, two states) gains the
        // child id: the run is the SAME record and the key does not move.
        let mut completed = report("call_report", "thread_1", "completed");
        completed.input = Some(serde_json::json!({
            "kind": "completed",
            "agentThreadId": "thread_1"
        }));
        let read = transcript(vec![anonymous, completed]);
        assert_eq!(read.runs.len(), 1, "one call is one run");
        assert_eq!(read.runs[0].run_id, "call_report", "the key never moves");
        assert_eq!(read.runs[0].child_id.as_deref(), Some("thread_1"));
        assert_eq!(read.runs[0].status, RunStatus::Completed);

        // Two call-keyed runs never share an empty identity.
        let mut other = report("call_other", "", "started");
        other.input = Some(serde_json::json!({"kind": "started", "agentThreadId": ""}));
        let read = transcript(vec![other]);
        assert_eq!(read.runs[0].run_id, "call_other");
        assert_ne!(read.runs[0].run_id, run.run_id);
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
            retirement.task.tool.call_id, "call_spawn",
            "the retirement names the run by its durable key"
        );
        assert_eq!(
            retirement.task.tool.name, "subagent",
            "the ledger's row stays subagent"
        );
    }
}
