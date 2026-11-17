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
pub(crate) fn derive_runs(messages: &mut [TranscriptMessage]) -> Vec<AgentRun> {
    let runs = {
        let read: &[TranscriptMessage] = messages;
        derive_runs_read(read)
    };
    // A report is an update to its run, and the launch panel is the record of
    // the request: once the client reports the run ended, the panel settles
    // with it rather than reading `Running` beside a finished run.
    let mut settled: Vec<(String, ToolStatus)> = Vec::new();
    let mut launch_ids: Vec<String> = runs
        .iter()
        .flat_map(|run| run.launched_by.iter().map(|launch| launch.call_id.clone()))
        .collect();
    launch_ids.sort();
    launch_ids.dedup();
    for launch_id in launch_ids {
        let group: Vec<&AgentRun> = runs
            .iter()
            .filter(|run| run.launched_by.iter().any(|launch| launch.call_id == launch_id))
            .collect();
        // One launch may name N children: it settles only when every one of
        // them is terminal, and a failure outranks a cancellation/complete. The
        // launch CALL's own status is a client fact too: a call the client
        // reported failed stays failed even if a later report completes the
        // child (ADR-0108 §10 records the derivation).
        if group.is_empty() || !group.iter().all(|run| run.status.is_terminal()) {
            continue;
        }
        let client_status = messages
            .iter()
            .flat_map(|message| &message.parts)
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == launch_id => Some(call.status.clone()),
                _ => None,
            });
        let failed = group.iter().any(|run| run.status == RunStatus::Failed)
            || matches!(client_status, Some(ToolStatus::Error));
        let cancelled = group.iter().any(|run| run.status == RunStatus::Cancelled)
            || matches!(&client_status, Some(ToolStatus::Other(reason)) if reason == "interrupted");
        let status = if failed {
            ToolStatus::Error
        } else if cancelled {
            ToolStatus::Other("interrupted".to_string())
        } else {
            ToolStatus::Completed
        };
        settled.push((launch_id, status));
    }
    if !settled.is_empty() {
        for message in messages.iter_mut() {
            for part in &mut message.parts {
                if let Part::Tool(call) = part
                    && let Some((_, status)) = settled
                        .iter()
                        .find(|(call_id, _)| call_id == &call.identity.call_id)
                {
                    call.status = status.clone();
                }
            }
        }
    }
    runs
}

fn derive_runs_read(messages: &[TranscriptMessage]) -> Vec<AgentRun> {
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
        // (spec #1057). A report naming no child is different: it creates the
        // call-keyed run below. Every ref this fold acts on here names exactly
        // one child; its status is the adapter's per-receiver declaration when
        // it made one, else the call's status.
        let names: Vec<(&str, Option<RunStatus>)> = refs
            .iter()
            .filter(|reference| !reference.run_id.is_empty())
            .map(|reference| (reference.run_id.as_str(), reference.status))
            .collect();
        match effect {
            DelegationEffect::Launch => {
                for (run_id, receiver_status) in names {
                    let run = resolve_launch(&mut runs, run_id, call);
                    // A launch is the run's naming fact: a report may have
                    // created it first with nothing but an id for a label, so
                    // the first launch fills the run's label and agent
                    // provenance (spec #1057's run shape).
                    if run.launched_by.is_empty() {
                        if let Some(label) = label_of(call) {
                            run.label = label;
                        }
                        if let Some(agent) = agent_of(call) {
                            run.agent = Some(agent);
                        }
                    }
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
                    // A launch must be *strictly* newer than the newest
                    // observation to revive a terminal run: the same
                    // millisecond is the old generation, and a resume with no
                    // clock cannot prove anything.
                    let launch_is_newest = match (call.started_at, run.observed_at) {
                        (Some(at), Some(seen)) => at > seen,
                        (Some(_), None) => true,
                        (None, _) => !run.status.is_terminal(),
                    };
                    if launch_is_newest {
                        run.status = receiver_status.unwrap_or_else(|| status_of(call));
                        if let Some(at) = call.started_at {
                            run.observed_at = Some(at);
                        }
                    }
                }
            }
            DelegationEffect::Address => {
                for (run_id, _) in names {
                    let run = resolve_or_create(&mut runs, run_id, call);
                    fill_child(run, run_id);
                    push_relation(&mut run.addressed_by, call);
                }
            }
            DelegationEffect::Report if names.is_empty() && !call.identity.call_id.is_empty() => {
                // A non-launch fact with no child id is still a fact about a
                // run: key it by the creating call, and fill the child id on a
                // later sighting (spec #1057's call-keyed case). A report is
                // the producer (a `subAgentActivity` with no thread id). An
                // empty id is no id: the report creates nothing.
                let run = resolve_call_keyed(&mut runs, call);
                push_relation(&mut run.reported_by, call);
                apply_report(run, call);
            }
            DelegationEffect::Report => {
                for (run_id, _) in names {
                    let run = resolve_or_create(&mut runs, run_id, call);
                    fill_child(run, run_id);
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

/// The run a non-launch fact creates when it names no child: keyed by the
/// call's own id, with `child_id` filled by a later sighting and `run_id`
/// never moving (spec #1057's "a run with no child id" case).
fn resolve_call_keyed<'a>(runs: &'a mut Vec<AgentRun>, call: &ToolCall) -> &'a mut AgentRun {
    let call_id = call.identity.call_id.as_str();
    // The run this call id created — whether or not a later sighting has since
    // named its child (the live id never moves). A childless report that
    // arrives after the child was filled is the same run's observation.
    if let Some(index) = runs.iter().position(|run| {
        !call_id.is_empty() && (run.run_id == call_id || run.task_run_id.as_deref() == Some(call_id))
    }) {
        return &mut runs[index];
    }
    let run_id = call_id.to_string();
    runs.push(AgentRun {
        run_id: run_id.clone(),
        task_run_id: (!call_id.is_empty()).then(|| call_id.to_string()),
        child_id: None,
        label: label_of(call).unwrap_or(run_id),
        agent: agent_of(call),
        // A report is an observation, not the child's start: the child may
        // have begun long before (the stale-evidence guard must not reject its
        // real turn). A later launch supplies the clock.
        started_at: None,
        status: RunStatus::Unknown,
        activity: None,
        launched_by: Vec::new(),
        addressed_by: Vec::new(),
        reported_by: Vec::new(),
        observed_at: None,
    });
    runs.last_mut().expect("just pushed")
}

/// The single live childless run a fact naming a child may adopt: the report
/// that created it named no child, and a later fact is the first to learn which
/// child it is (ADR-0108 §1). `None` when none or several are live — adopting
/// one of two would be a guess.
fn sole_childless_candidate(runs: &[AgentRun]) -> Option<usize> {
    let candidates: Vec<usize> = runs
        .iter()
        .enumerate()
        .filter(|(_, run)| run.child_id.is_none() && run.launched_by.is_empty() && run.is_live())
        .map(|(index, _)| index)
        .collect();
    match candidates.as_slice() {
        [index] => Some(*index),
        _ => None,
    }
}

/// The run a launch names: an existing record, else a childless call-keyed run
/// it can adopt (the report that created it named no child), else a new run.
/// Adoption only happens when exactly one childless run is live — two
/// candidates would be a guess.
fn resolve_launch<'a>(runs: &'a mut Vec<AgentRun>, run_id: &str, call: &ToolCall) -> &'a mut AgentRun {
    let existing = runs
        .iter()
        .position(|run| run.run_id == run_id || run.child_id.as_deref() == Some(run_id));
    if let Some(index) = existing {
        return &mut runs[index];
    }
    // Only a live childless run can be adopted: a terminal one may already own
    // an ending under its call id, and moving its key would split that record.
    if let Some(index) = sole_childless_candidate(runs) {
        let run = &mut runs[index];
        run.child_id = Some(run_id.to_string());
        // The child id is the run's durable key once it is known: a call-keyed
        // run's provisional id would otherwise make the key depend on where
        // the read window began (ADR-0080's amendment).
        run.task_run_id = Some(run_id.to_string());
        return run;
    }
    resolve_or_create(runs, run_id, call)
}

/// The run one delegation call names by `run_id`, created when no fact has
/// named it yet. The live identity is the child id (spec #1057), and a later
/// sighting resolves the same record by either its live identity or its child
/// id, so `run_id` never moves once minted.
///
/// `task_run_id` (ADR-0080's durable key) is the run's **child id** whenever
/// one is named. A launch/address names a child to create a run, and that id is
/// the only key every read window agrees on: a window starting at the launch,
/// at a `wait`, or at a report must mint the same durable identity, and a
/// call-id key would differ by which call the window happens to begin at (and
/// could not identify two runs named by one multi-child fact). A report that
/// names NO child is the call-keyed arm above: its creating call id is the
/// provisional key, moved to the child id by a later sighting (ADR-0108 §1).
fn resolve_or_create<'a>(runs: &'a mut Vec<AgentRun>, run_id: &str, call: &ToolCall) -> &'a mut AgentRun {
    if let Some(index) = runs.iter().position(|run| {
        run.run_id == run_id
                || run.child_id.as_deref() == Some(run_id)
                // The same call that created a childless run now names its
                // thread: resolve that run instead of minting a second one.
                || (run.child_id.is_none()
                    && !call.identity.call_id.is_empty()
                    && run.run_id == call.identity.call_id)
    }) {
        return &mut runs[index];
    }
    // A fact naming a child may be the first to learn which child a report's
    // call-keyed run is (ADR-0108 §1): adopt the single live childless run, so
    // one child never owns two runs. Two candidates would be a guess.
    if let Some(index) = sole_childless_candidate(runs) {
        return &mut runs[index];
    }
    runs.push(AgentRun {
        run_id: run_id.to_string(),
        task_run_id: Some(run_id.to_string()),
        child_id: Some(run_id.to_string()),
        label: label_of(call).unwrap_or_else(|| run_id.to_string()),
        agent: agent_of(call),
        // Only a LAUNCH dates the child (the `Launch` arm fills it): an
        // addressing fact names a run whose window may begin at the `wait`,
        // and its clock says nothing about when the child started — the
        // stale-evidence guard must not use it.
        started_at: None,
        // A non-launch fact (a `wait`/`sendInput` window) names the run but
        // states nothing about its life: start Unknown/live. Only a launch or a
        // report supplies the status.
        status: RunStatus::Unknown,
        activity: None,
        observed_at: None,
        launched_by: Vec::new(),
        addressed_by: Vec::new(),
        reported_by: Vec::new(),
    });
    runs.last_mut().expect("just pushed")
}

/// Fill a call-keyed run's child id from a later sighting without moving
/// `run_id` (spec #1057: the live identity is minted once). A TERMINAL run
/// keeps its durable key too: an ending may already be recorded under the call
/// id, and moving the key would split that record (ADR-0108 §1's "a terminal
/// childless run is never adopted").
fn fill_child(run: &mut AgentRun, child_id: &str) {
    if run.child_id.is_none() && !child_id.is_empty() {
        run.child_id = Some(child_id.to_string());
        // The child id is the durable key once named (ADR-0080's amendment):
        // the provisional call id must not survive into a window-dependent
        // `ending_key`.
        if run.is_live() {
            run.task_run_id = Some(child_id.to_string());
        }
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
    let status = status_of(call);
    // A terminal statement latches: only a report that proves it is newer (a
    // clock strictly after the last observation) may revive the run. A start
    // with no clock (or the same second) cannot establish a new generation.
    let newer = match (run.observed_at, call.started_at) {
        (Some(seen), Some(at)) => at > seen,
        (None, Some(_)) => true,
        (_, None) => false,
    };
    if run.status.is_terminal() && !status.is_terminal() && !newer {
        return;
    }
    if call.started_at.is_some() {
        run.observed_at = call.started_at;
    }
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
    // The newest live observation is the phase's own clock: a later
    // `interacted`/`resumed` report moves it forward, never back.
    if let Some(at) = call.started_at
        && activity.since.is_none_or(|since| at > since)
    {
        activity.since = Some(at);
    }
}

/// The client's status for the call that created or resumed the run. A launch
/// call's `Completed` means the client's statement is "this run is done" — a
/// foreground `task` returns only when its child ends, and a resumed task that
/// settled is a finished child. A client whose launch is only a handle states
/// the child's later state through a `subAgentActivity` report.
pub(crate) fn status_of(call: &ToolCall) -> RunStatus {
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
        // Only the child's own output is a phase: a user prompt it has received
        // but not answered is `waiting`, not `responding` — so the previous
        // turn's part and call no longer describe this one.
        if message.role != super::MessageRole::Assistant {
            newest_part = None;
            current = None;
            continue;
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
    // A user message newer than the last assistant output is a new generation
    // of the same child: the previous turn's finish no longer states this one.
    // Compaction/Other messages between them do not hide the boundary.
    let last_user = transcript
        .messages
        .iter()
        .rposition(|message| message.role == super::MessageRole::User);
    let last_assistant = transcript
        .messages
        .iter()
        .rposition(|message| message.role == super::MessageRole::Assistant);
    if let Some(user) = last_user
        && last_assistant.is_none_or(|assistant| user > assistant)
    {
        return None;
    }
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
        TurnAnchor, TurnSettle,
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
            "interrupted" => ToolStatus::Other("interrupted".to_string()),
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

    /// A report is an update to its run, and the launch panel is the record of
    /// the request: once the client reports the run ended, the panel settles
    /// with it rather than reading `Running` beside a finished run. A failure
    /// the CLIENT reported on the call itself stays: it is a fact about the
    /// request, not a stale run state.
    #[test]
    fn a_terminal_report_settles_the_launch_call() {
        let launch = call(
            "call_task",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(10),
        );
        let mut report = report("call_report", "ses_child", "completed");
        report.started_at = Some(20);
        let read = SessionTranscript::new(vec![TranscriptMessage {
            id: MessageId::new("msg_1"),
            role: super::super::MessageRole::Assistant,
            time: Some(MessageTime {
                created: 10,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Tool(launch), Part::Tool(report)],
        }]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
        let launch_status = read.messages[0]
            .parts
            .iter()
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == "call_task" => Some(call.status.clone()),
                _ => None,
            })
            .expect("the launch call");
        assert_eq!(launch_status, ToolStatus::Completed);
    }

    /// The client's failure on the launch call itself is sticky: a later report
    /// that completes the child must not paint ✅ over the ❌ the client stated.
    #[test]
    fn a_client_failed_launch_keeps_its_panel() {
        let launch = call(
            "call_task",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Error,
            Some(10),
        );
        let mut report = report("call_report", "ses_child", "completed");
        report.started_at = Some(20);
        let read = SessionTranscript::new(vec![TranscriptMessage {
            id: MessageId::new("msg_1"),
            role: super::super::MessageRole::Assistant,
            time: Some(MessageTime {
                created: 10,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Tool(launch), Part::Tool(report)],
        }]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
        let launch_status = read.messages[0]
            .parts
            .iter()
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == "call_task" => Some(call.status.clone()),
                _ => None,
            })
            .expect("the launch call");
        assert_eq!(launch_status, ToolStatus::Error, "the client's ❌ stays");
    }

    /// A report naming no child is still a run: keyed by its own call, with no
    /// child id until a later sighting.
    #[test]
    fn a_report_naming_no_child_creates_a_call_keyed_run() {
        let mut call = call(
            "call_report",
            DelegationEffect::Report,
            &[],
            ToolStatus::Running,
            Some(5),
        );
        call.input = Some(serde_json::json!({"kind": "started"}));
        let read = transcript(vec![call]);
        assert_eq!(read.runs.len(), 1);
        assert_eq!(read.runs[0].run_id, "call_report");
        assert_eq!(read.runs[0].task_run_id.as_deref(), Some("call_report"));
        assert_eq!(read.runs[0].child_id, None);
        assert_eq!(read.runs[0].status, RunStatus::Running);
    }

    /// A launch names a run a report created first: its label and agent fill
    /// the report's identity-only record, so the ledger and `/sub` read the
    /// launch's words (spec #1057's run shape).
    #[test]
    fn a_launch_fills_a_report_created_runs_label() {
        let mut report = call(
            "call_report",
            DelegationEffect::Report,
            &["thread_1"],
            ToolStatus::Running,
            Some(5),
        );
        report.input = Some(serde_json::json!({"kind": "started"}));
        let mut launch = call(
            "call_spawn",
            DelegationEffect::Launch,
            &["thread_1"],
            ToolStatus::Running,
            Some(6),
        );
        launch.input = Some(serde_json::json!({
            "description": "review the diff",
            "subagent_type": "reviewer"
        }));
        let read = transcript(vec![report, launch]);
        assert_eq!(read.runs.len(), 1, "one child is one run");
        assert_eq!(read.runs[0].label, "review the diff");
        assert_eq!(read.runs[0].agent.as_deref(), Some("reviewer"));
    }

    /// An addressing fact names a run whose window may begin at the `wait`; its
    /// clock is not the child's start, so the stale-evidence guard must accept
    /// a terminal child that began earlier.
    #[test]
    fn an_address_created_run_takes_no_start_clock() {
        let address = call(
            "call_wait",
            DelegationEffect::Address,
            &["thread_1"],
            ToolStatus::Running,
            Some(12_000),
        );
        let mut read = transcript(vec![address]);
        assert_eq!(read.runs.len(), 1);
        assert_eq!(
            read.runs[0].started_at, None,
            "an address does not date the child"
        );
        assert!(read.apply_run_child_evidence(
            "thread_1",
            ChildEvidence::Terminal {
                completed_at: 12_010,
                started_at: Some(12_001)
            }
        ));
        assert!(read.runs.is_empty(), "the child's own turn ends it");
    }

    /// A later ADDRESS naming the child fills the call-keyed run's identity
    /// instead of minting a second run. `sendInput`/`wait` do not relaunch, but
    /// they are the first fact to learn which child the report meant.
    #[test]
    fn an_address_adopts_the_childless_run() {
        let activity = call(
            "call_activity",
            DelegationEffect::Report,
            &[],
            ToolStatus::Running,
            Some(5),
        );
        let address = call(
            "call_wait",
            DelegationEffect::Address,
            &["thread_1"],
            ToolStatus::Running,
            Some(6),
        );
        let read = transcript(vec![activity, address]);
        assert_eq!(read.runs.len(), 1, "one run, filled not split");
        assert_eq!(read.runs[0].run_id, "call_activity", "the live id never moves");
        assert_eq!(read.runs[0].child_id.as_deref(), Some("thread_1"));
        assert_eq!(read.runs[0].task_run_id.as_deref(), Some("thread_1"));
        assert_eq!(read.runs[0].addressed_by[0].call_id, "call_wait");
    }

    /// An id-less report that arrives AFTER a sighting filled the child is the
    /// same run's observation, not a second childless run: the activity item
    /// keeps the call id it was created under.
    #[test]
    fn an_empty_report_after_the_child_is_named_is_the_same_run() {
        let activity = call(
            "call_activity",
            DelegationEffect::Report,
            &[],
            ToolStatus::Running,
            Some(5),
        );
        let launch = call(
            "call_spawn",
            DelegationEffect::Launch,
            &["thread_1"],
            ToolStatus::Running,
            Some(6),
        );
        let done = call(
            "call_activity",
            DelegationEffect::Report,
            &[],
            ToolStatus::Completed,
            Some(7),
        );
        let read = transcript(vec![activity, launch, done]);
        assert_eq!(read.runs.len(), 1, "the same call id is one run: {:?}", read.runs);
        assert_eq!(read.runs[0].child_id.as_deref(), Some("thread_1"));
        assert_eq!(read.runs[0].status, RunStatus::Completed);
    }

    /// A terminal call-keyed run keeps its durable key: an ending may already
    /// be recorded under the call id, so a later child sighting fills the id
    /// without splitting that record (ADR-0108 §1).
    #[test]
    fn a_late_child_sighting_does_not_split_a_terminal_runs_key() {
        let done = call(
            "call_activity",
            DelegationEffect::Report,
            &[],
            ToolStatus::Completed,
            Some(5),
        );
        let named = call(
            "call_activity",
            DelegationEffect::Report,
            &["thread_1"],
            ToolStatus::Completed,
            Some(6),
        );
        let read = transcript(vec![done, named]);
        assert_eq!(read.runs.len(), 1, "the same call id is one run");
        assert_eq!(read.runs[0].child_id.as_deref(), Some("thread_1"));
        assert_eq!(
            read.runs[0].ending_key(),
            "call_activity",
            "a terminal run's recorded key never moves"
        );
    }

    /// The durable key is window-independent even when the child id is learned
    /// after the run was created call-keyed.
    #[test]
    fn an_adopted_call_keyed_run_keeps_one_window_stable_key() {
        let mut activity = call(
            "activity_1",
            DelegationEffect::Report,
            &[],
            ToolStatus::Running,
            Some(5),
        );
        activity.input = Some(serde_json::json!({"kind": "started"}));
        let spawn = call(
            "spawn_1",
            DelegationEffect::Launch,
            &["thread_1"],
            ToolStatus::Running,
            Some(6),
        );
        let full = transcript(vec![activity, spawn.clone()]);
        let launch_only = transcript(vec![spawn]);
        assert_eq!(
            full.runs[0].ending_key(),
            launch_only.runs[0].ending_key(),
            "a window beginning at the launch mints the same durable key"
        );
        assert_eq!(full.runs[0].ending_key(), "thread_1");
    }

    /// A later launch adopts the one childless run a report created, so the
    /// child's run is never split in two.
    #[test]
    fn a_later_launch_adopts_the_childless_run() {
        let mut activity = call(
            "activity_1",
            DelegationEffect::Report,
            &[],
            ToolStatus::Running,
            Some(5),
        );
        activity.input = Some(serde_json::json!({"kind": "started"}));
        let spawn = call(
            "spawn_1",
            DelegationEffect::Launch,
            &["thread_1"],
            ToolStatus::Running,
            Some(6),
        );
        let read = transcript(vec![activity, spawn]);
        assert_eq!(read.runs.len(), 1, "one run, adopted not split");
        assert_eq!(read.runs[0].run_id, "activity_1", "the live id never moves");
        assert_eq!(read.runs[0].child_id.as_deref(), Some("thread_1"));
        assert_eq!(
            read.runs[0].task_run_id.as_deref(),
            Some("thread_1"),
            "the durable key is the child id once it is known"
        );
        assert_eq!(read.runs[0].launched_by[0].call_id, "spawn_1");
    }

    /// The same activity item's later sighting fills the call-keyed run's
    /// child id instead of minting a second run.
    #[test]
    fn a_call_keyed_run_fills_its_child_on_the_same_item() {
        let mut start = call(
            "call_activity",
            DelegationEffect::Report,
            &[],
            ToolStatus::Running,
            Some(5),
        );
        start.input = Some(serde_json::json!({"kind": "started"}));
        let mut done = call(
            "call_activity",
            DelegationEffect::Report,
            &["thread_1"],
            ToolStatus::Completed,
            Some(9),
        );
        done.input = Some(serde_json::json!({"kind": "completed"}));
        let read = transcript(vec![start, done]);
        assert_eq!(read.runs.len(), 1, "one run, never a second record");
        assert_eq!(read.runs[0].run_id, "call_activity");
        assert_eq!(read.runs[0].child_id.as_deref(), Some("thread_1"));
        assert_eq!(read.runs[0].task_run_id.as_deref(), Some("thread_1"));
        assert_eq!(read.runs[0].status, RunStatus::Completed);
    }

    /// A clockless resume cannot revive a terminal run: nothing proves it is a
    /// new generation, and reviving it would let pre-resume evidence retire it.
    #[test]
    fn a_clockless_launch_does_not_revive_a_terminal_run() {
        let launch = call(
            "call_task",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(1),
        );
        let mut completed = report("call_done", "ses_child", "completed");
        completed.started_at = Some(4);
        let mut resumed = call(
            "call_resume",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            None,
        );
        resumed.status = ToolStatus::Running;
        let read = transcript(vec![launch, completed, resumed]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
        assert_eq!(
            read.runs[0].launched_by.len(),
            2,
            "the resume is still a launch relation"
        );
    }

    /// An evidence ending settles the launch panel too: it must not keep
    /// reading live beside a run that just ended.
    #[test]
    fn a_retired_run_settles_its_launch_call() {
        let mut read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(1_000),
        )]);
        assert!(read.apply_run_child_evidence(
            "ses_child",
            ChildEvidence::Terminal {
                completed_at: 7_000,
                started_at: None
            }
        ));
        assert!(read.runs.is_empty());
        let launch_status = read.messages[0]
            .parts
            .iter()
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == "call_spawn" => Some(call.status.clone()),
                _ => None,
            })
            .expect("the launch call");
        assert_eq!(launch_status, ToolStatus::Completed);
    }

    /// A terminal run from an earlier Turn is history: it does not get an
    /// evidence read (which would mint a completion entry for every old child),
    /// and it must not keep the newest Turn waiting on an ending no read will
    /// ever produce.
    #[test]
    fn a_terminal_run_from_an_earlier_turn_is_history() {
        let user = |id: &str, at: i64| TranscriptMessage {
            id: MessageId::new(id),
            role: super::super::MessageRole::User,
            time: Some(MessageTime {
                created: at,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Text(crate::backend::TextPart {
                text: "prompt".into(),
                started_at: Some(at),
            })],
        };
        let launch = |id: &str, child: &str, at: i64, status: ToolStatus| TranscriptMessage {
            id: MessageId::new(id),
            role: super::super::MessageRole::Assistant,
            time: Some(MessageTime {
                created: at,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Tool(call(
                &format!("call_{id}"),
                DelegationEffect::Launch,
                &[child],
                status,
                Some(at),
            ))],
        };
        let read = SessionTranscript::new(vec![
            user("msg_u1", 1_000),
            launch("msg_old", "ses_old", 2_000, ToolStatus::Completed),
            user("msg_u2", 5_000),
            launch("msg_new", "ses_new", 6_000, ToolStatus::Completed),
        ]);
        let anchor_b = TurnAnchor {
            message_id: MessageId::new("msg_u2"),
            created_ms: 5_000,
        };
        assert_eq!(
            read.run_evidence_suspects(Some(&anchor_b)),
            vec![("ses_new".to_string(), "ses_new".to_string())],
            "only the current Turn's terminal run owes evidence"
        );
        assert_eq!(
            read.settle_with(Some(&anchor_b), true),
            TurnSettle::Waiting,
            "the newest Turn's own terminal run still owes its evidence"
        );
        let history_only = SessionTranscript::new(vec![
            user("msg_u1", 1_000),
            launch("msg_old", "ses_old", 2_000, ToolStatus::Completed),
            user("msg_u2", 5_000),
        ]);
        assert_eq!(
            history_only.settle_with(Some(&anchor_b), true),
            TurnSettle::Complete,
            "history cannot wedge the newest Turn on evidence no read will fetch"
        );

        // A run with no start cannot be placed: the window that read it may
        // have lost its clock, so it is NOT history. It keeps owing its child
        // read and keeps the Turn waiting, or its ending would never be minted.
        let mut unclocked = call(
            "call_unplaced",
            DelegationEffect::Launch,
            &["ses_unplaced"],
            ToolStatus::Completed,
            Some(2_000),
        );
        unclocked.started_at = None;
        let unplaced = SessionTranscript::new(vec![
            user("msg_u1", 1_000),
            TranscriptMessage {
                id: MessageId::new("msg_unplaced"),
                role: super::super::MessageRole::Assistant,
                time: Some(MessageTime {
                    created: 2_000,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Tool(unclocked)],
            },
            user("msg_u2", 5_000),
        ]);
        assert_eq!(
            unplaced.run_evidence_suspects(Some(&anchor_b)),
            vec![("ses_unplaced".to_string(), "ses_unplaced".to_string())],
            "an unplaceable terminal run is not history"
        );
        assert_eq!(
            unplaced.settle_with(Some(&anchor_b), true),
            TurnSettle::Waiting,
            "it owes an evidence ending the Turn must not skip"
        );
    }

    /// A shared launch settles only when every child it named is terminal, and
    /// a failure outranks a cancellation or a complete.
    #[test]
    fn a_multi_child_launch_settles_with_the_last_terminal_run() {
        let launch = call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_a", "ses_b"],
            ToolStatus::Running,
            Some(10),
        );
        let a_done = {
            let mut report = report("call_a", "ses_a", "completed");
            report.started_at = Some(20);
            report
        };
        let b_live = {
            let mut report = report("call_b", "ses_b", "interrupted");
            report.started_at = Some(21);
            report
        };
        let read = SessionTranscript::new(vec![TranscriptMessage {
            id: MessageId::new("msg_1"),
            role: super::super::MessageRole::Assistant,
            time: Some(MessageTime {
                created: 10,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Tool(launch), Part::Tool(a_done), Part::Tool(b_live)],
        }]);
        let launch = read.messages[0]
            .parts
            .iter()
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == "call_spawn" => Some(call.status.clone()),
                _ => None,
            })
            .expect("the launch call");
        assert_eq!(
            launch,
            ToolStatus::Other("interrupted".to_string()),
            "a cancelled child names the settled launch's word"
        );
    }

    /// A child that has received a prompt but not answered is `waiting`: only
    /// its own output is a phase, and a new user turn makes the previous
    /// turn's finish stale.
    #[test]
    fn a_user_prompt_alone_is_waiting_not_responding() {
        let read = SessionTranscript::new(vec![
            TranscriptMessage {
                id: MessageId::new("msg_assistant"),
                role: super::super::MessageRole::Assistant,
                time: Some(MessageTime {
                    created: 1,
                    completed: Some(2),
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::StepFinish(StepFinish {
                    reason: FinishReason::Stop,
                })],
            },
            TranscriptMessage {
                id: MessageId::new("msg_user"),
                role: super::super::MessageRole::User,
                time: Some(MessageTime {
                    created: 5,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Text(crate::backend::TextPart {
                    text: "resume please".to_string(),
                    started_at: Some(5),
                })],
            },
        ]);
        assert!(!child_run_finished(&read), "a new user turn is not finished");
        assert_eq!(
            child_run_activity(&read).map(|activity| activity.phase),
            Some(Some(Phase::Waiting))
        );
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
    fn an_unnamed_address_mints_nothing() {
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
        let read = transcript(vec![wait, empty_address]);
        assert!(
            read.runs.is_empty(),
            "an addressing call that names no run mints no run: {:?}",
            read.runs
        );
        // A report naming no child is still a fact about a run: it is keyed by
        // its own call (the call-keyed case).
        let read = transcript(vec![empty_report]);
        assert_eq!(read.runs.len(), 1);
        assert_eq!(read.runs[0].run_id, "call_activity");
        assert_eq!(read.runs[0].child_id, None);
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
        assert!(read.run_evidence_suspects(None).is_empty());
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
    /// ends the run, an interrupted one cancels it, and an unknown/new activity
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
        assert_eq!(read.runs[0].status, RunStatus::Cancelled);

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

    /// A terminal statement latches even without clocks: a start that cannot
    /// prove it is newer cannot revive a finished child.
    #[test]
    fn a_clockless_start_cannot_revive_a_terminal_run() {
        let mut completed = report("call_done", "thread_1", "completed");
        completed.started_at = Some(3);
        let mut clockless = report("call_started_none", "thread_1", "started");
        clockless.started_at = None;
        let mut same_second = report("call_started_same", "thread_1", "started");
        same_second.started_at = Some(3);
        let read = transcript(vec![completed, clockless, same_second]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
        assert!(read.runs[0].activity.is_none());
        assert_eq!(read.runs[0].reported_by.len(), 3);
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
        completed.started_at = Some(4_000);
        let mut read = transcript(vec![
            call(
                "call_1",
                DelegationEffect::Launch,
                &["ses_child"],
                ToolStatus::Running,
                Some(1_000),
            ),
            completed,
            call(
                "call_2",
                DelegationEffect::Launch,
                &["ses_child"],
                ToolStatus::Running,
                Some(5_000),
            ),
        ]);
        let run = &read.runs[0];
        assert_eq!(run.started_at, Some(5_000), "the resume moves the run's start");
        assert_eq!(
            run.status,
            RunStatus::Running,
            "the resume is the newest client statement"
        );
        assert!(run.is_live());

        assert!(
            !read.apply_run_child_evidence(
                "ses_child",
                ChildEvidence::Terminal {
                    // The completion lands AFTER the resume, but the turn it
                    // belongs to started before it.
                    completed_at: 6_000,
                    started_at: Some(4_000)
                }
            ),
            "a pre-resume turn is older than the resumed start"
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

    /// A run first seen through an addressing receipt takes no status from it:
    /// a `wait`/`sendInput` window is not a statement about the child's life.
    /// Only a later launch or report supplies the status.
    #[test]
    fn an_address_only_window_keeps_the_run_live() {
        let read = transcript(vec![call(
            "call_wait",
            DelegationEffect::Address,
            &["thread_1"],
            ToolStatus::Completed,
            Some(1),
        )]);
        assert_eq!(read.runs.len(), 1);
        assert_eq!(read.runs[0].status, RunStatus::Unknown);
        assert!(read.runs[0].is_live(), "the receipt did not end the run");

        let read = transcript(vec![
            call(
                "call_wait",
                DelegationEffect::Address,
                &["thread_1"],
                ToolStatus::Completed,
                Some(1),
            ),
            call(
                "call_spawn",
                DelegationEffect::Launch,
                &["thread_1"],
                ToolStatus::Running,
                Some(2),
            ),
        ]);
        assert_eq!(read.runs[0].status, RunStatus::Running);

        let read = transcript(vec![
            call(
                "call_wait",
                DelegationEffect::Address,
                &["thread_1"],
                ToolStatus::Completed,
                Some(1),
            ),
            report("call_done", "thread_1", "completed"),
        ]);
        assert_eq!(read.runs[0].status, RunStatus::Completed);
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
            read.run_evidence_suspects(None),
            vec![("thread_1".to_string(), "thread_1".to_string())],
            "the child read still has to confirm the end"
        );

        let mut read = read;
        assert!(read.apply_run_child_evidence(
            "thread_1",
            ChildEvidence::Terminal {
                completed_at: 3,
                started_at: None
            }
        ));
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
        assert!(read.run_evidence_suspects(None).is_empty());
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
        assert!(read.apply_run_child_evidence(
            "ses_child",
            ChildEvidence::Terminal {
                completed_at: 9,
                started_at: None
            }
        ));
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
                completed_at: 1_700_000_002_000,
                started_at: None
            }
        ));
        assert!(read.runs.is_empty(), "the run retires on its child's evidence");
        assert_eq!(read.task_retirements[0].finished_at, Some(1_700_000_002_000));
    }

    /// Codex's turn start is the ordering fact, at whole-second granularity: a
    /// turn that began in an earlier second is the previous generation's; a
    /// same-second start is the ambiguity ADR-0108 accepts. With no turn start
    /// reported, the completion clock is the fallback.
    #[test]
    fn a_turn_start_orders_the_child_evidence() {
        let mut read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(1_500),
        )]);
        assert!(read.apply_run_child_evidence(
            "ses_child",
            ChildEvidence::Terminal {
                // The wire serializes the turn start into second 1; the run
                // started at 1_500 ms in the same second. ADR-0108 records the
                // ambiguity: a same-second start is accepted.
                completed_at: 2_000,
                started_at: Some(1_000)
            }
        ));
        assert!(read.runs.is_empty());

        let mut read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(5_000),
        )]);
        assert!(
            !read.apply_run_child_evidence(
                "ses_child",
                ChildEvidence::Terminal {
                    completed_at: 6_000,
                    started_at: Some(4_000)
                }
            ),
            "a previous turn is stale even when its completion lands after the resume"
        );

        let mut read = transcript(vec![call(
            "call_spawn",
            DelegationEffect::Launch,
            &["ses_child"],
            ToolStatus::Running,
            Some(1_500),
        )]);
        assert!(
            !read.apply_run_child_evidence(
                "ses_child",
                ChildEvidence::Terminal {
                    completed_at: 999,
                    started_at: None
                }
            ),
            "no turn start falls back to the completion clock"
        );
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
