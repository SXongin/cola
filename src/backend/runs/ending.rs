//! The durable **Task Ending** types (ADR-0080, ADR-0111 §2): one run's
//! ending classes, entry state, evidence and the identity an entry is
//! re-synthesized from.
//!
//! These are the facts `RunEndingStore` stores and `RunLifecycle` (ADR-0111)
//! owns; the sidecar that persists them and the exactly-once entry bookkeeping
//! stay in [`crate::bridge::chain`].

use serde::{Deserialize, Serialize};

use super::RunKey;

/// One **Task Ending**'s class (ADR-0080): what ended a Background Task run. A
/// run carries at most one ending per class, because the classes are different
/// claims about it — an [`TaskEndingClass::Evidence`] ending says the run ENDED,
/// so it suppresses a late **Wake**'s completion entry (#659), while a
/// [`TaskEndingClass::Cleanup`] claims nothing about how the run ended, so a
/// cleaned run's late Wake is its own ending (#590).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskEndingClass {
    /// An evidence ending: a **Runtime Reconciliation** verdict, a shell the
    /// runtime no longer knows, or a subagent's own terminal transcript.
    Evidence,
    /// The user's own **Cleanup** (spec #588, #590).
    Cleanup,
    /// The backend's own completion **Wake**, recorded for a run that has no
    /// evidence ending.
    Wake,
    /// The pre-#659 `runtime:<call_id>` spelling, translated once at load: read
    /// by the retirement's own dedup, never by the Wake gate (#590).
    Legacy,
}

/// What a derivation had as evidence for a run's end (ADR-0080): the detail its
/// class does not carry. Kept as the derivation reported it, so a later read
/// synthesizes the entry without re-reading the transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskEndingEvidence {
    /// The runtime's terminal shell verdict, verbatim — a status this build does
    /// not know stays as it came.
    Ended(String),
    /// The runtime has no record of the task at all.
    Lost,
    /// The child session's own newest assistant message ended the run (#591).
    ChildEnded,
    /// The state a **Wake** reported (`completed` / `cancelled` / `error`),
    /// `None` when it reported none.
    Wake(Option<String>),
}

/// An ending's entry state (ADR-0080). Monotonic: it only ever moves AWAY from
/// [`TaskEndingState::Owed`] — a confirmed card write shows it, or the
/// saturation cap surrenders it — so no later write can return an entry to
/// `Owed` and re-offer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskEndingState {
    /// The entry the next card still renders.
    Owed,
    /// A confirmed card write displayed the entry.
    Shown,
    /// The entry was given up (its fact stays), for one of two reasons: the
    /// saturation cap surrendered it — the one ratified loss, for a prolonged
    /// carrier failure — or the class-agnostic entry gate did, because the run's
    /// entry was already shown in some other class and this entry was therefore
    /// never renderable (no loss: nothing a card could have shown was given up).
    /// Only an owed ending is ever rendered, so a surrendered one is never
    /// re-offered.
    Surrendered,
}

/// The identity a [`BackgroundTask`] is re-synthesized from (ADR-0080): what an
/// entry needs when the read no longer carries the run's launch, so the
/// synthesis never depends on the transcript's own scope. The label and the
/// start clock are captured when the ending is DERIVED — that read necessarily
/// carries the launch — which makes a later card's entry immune to compaction
/// and to a truncated (V2 page-capped) read.
///
/// [`BackgroundTask`]: crate::backend::BackgroundTask
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskRunIdentity {
    /// The tool that launched the run (`shell` / `subagent`) — the kind the
    /// entry renders as.
    #[serde(default)]
    pub(crate) tool: Option<String>,
    /// The launch's own tool call id.
    #[serde(default)]
    pub(crate) call_id: Option<String>,
    /// The shell the run executes in.
    #[serde(default)]
    pub(crate) shell_id: Option<String>,
    /// The child session a subagent runs in.
    #[serde(default)]
    pub(crate) child_id: Option<String>,
    /// When the run started.
    #[serde(default)]
    pub(crate) started_at: Option<i64>,
    /// The label the launch's input carried, when it named one.
    #[serde(default)]
    pub(crate) label: Option<String>,
}

impl TaskRunIdentity {
    /// The [`BackgroundTask`] this identity re-synthesizes (ADR-0080), so a
    /// durable ending's entry renders without the launch being in the read. The
    /// run's stored key stands in for a launch call id the identity never
    /// captured (a Wake's ending).
    pub(crate) fn task(&self, fallback_key: &str) -> crate::backend::BackgroundTask {
        crate::backend::BackgroundTask {
            tool: crate::backend::ToolIdentity {
                name: self.tool.clone().unwrap_or_else(|| "shell".to_string()),
                call_id: self.call_id.clone().unwrap_or_else(|| fallback_key.to_string()),
            },
            shell_id: self.shell_id.clone(),
            child_id: self.child_id.clone(),
            started_at: self.started_at,
            ..Default::default()
        }
    }
}

/// One durable **Task Ending** (ADR-0080): a claim about how one Background Task
/// run ended, recorded at derivation — except a **Wake** completion's, recorded
/// where its completion entry is committed to a card's render — and outliving
/// the record it was first written beside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskEnding {
    pub(crate) class: TaskEndingClass,
    pub(crate) state: TaskEndingState,
    /// When the run ended — the Wake's own server time, the runtime's reported
    /// completion time, or `None` for an ending the runtime could not stamp.
    #[serde(default)]
    pub(crate) finished_at: Option<i64>,
    /// The class's own evidence, when the derivation reported any.
    #[serde(default)]
    pub(crate) evidence: Option<TaskEndingEvidence>,
    /// The identity a Background Task is re-synthesized from.
    #[serde(default)]
    pub(crate) identity: TaskRunIdentity,
}

impl TaskEnding {
    /// A just-derived ending: nothing has shown it yet, so it is OWED until a
    /// confirmed card write shows it (ADR-0080).
    pub(crate) fn owed(class: TaskEndingClass, identity: TaskRunIdentity) -> Self {
        Self {
            class,
            state: TaskEndingState::Owed,
            finished_at: None,
            evidence: None,
            identity,
        }
    }
}

/// One run's record in the `endings` section (ADR-0080): the ids the run is
/// known by and at most one ending per class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskRunRecord {
    /// The run's identity: its stored key and every id it is known by.
    pub(crate) run: RunKey,
    #[serde(default)]
    pub(crate) endings: Vec<TaskEnding>,
}

impl TaskRunRecord {
    /// Whether the record still owes an entry: an ending no card has shown and
    /// neither cause has surrendered — not the saturation cap, and not the
    /// class-agnostic entry gate (a run whose entry was already shown in ANY
    /// class; see `insert_ending` / `mark_shown`). Only an OWED ending is
    /// ever rendered, and only a record with no owed ending is the bound's
    /// eviction candidate.
    pub(crate) fn owes(&self) -> bool {
        self.endings
            .iter()
            .any(|ending| ending.state == TaskEndingState::Owed)
    }
}
