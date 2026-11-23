//! A running child run's liveness as a panel and the ledger show it
//! (ADR-0054). Display-only data — the Bridge gathers it, the Platform formats
//! it — so the panel stays a view over the call plus one read-only line.

use crate::backend::{Phase, RunActivity, RunWaits};

use super::super::AwaitingAction;
use super::super::shell::fmt_elapsed;
use super::super::vocab::{self, phase_token};

/// A live `task`/`subagent` call's child-session liveness: what the child is
/// doing right now, as the panel title shows it. It is the card's view of the
/// run's neutral [`RunActivity`] (`from_run`), so the panel, the ledger and the
/// completion entry cannot disagree about one run.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLiveness {
    /// The child's current activity: its newest live tool, or the phase its
    /// newest part implies.
    pub activity: ChildActivity,
    /// Epoch ms of the child's newest observed activity: the timer a phase
    /// (no tool running) counts from. Stored as a time, not an age, so a
    /// failed read never freezes a made-up "5s". `None` when the run's
    /// activity carries no clock at all: the fragment then shows the phase
    /// word with no age rather than an age measured from the epoch.
    pub last_activity_ms: Option<i64>,
    /// The child's pending wait, in the header's own vocabulary (ADR-0054).
    pub wait: Option<AwaitingAction>,
}

/// What a child session is doing right now, as its transcript reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildActivity {
    /// The child's newest live tool. `started_at` is the server clock the
    /// duration counts from; a call with no clock yet shows its name alone.
    Tool { name: String, started_at: Option<i64> },
    /// Between steps: the phase its newest part implies
    /// (`waiting`/`reasoning`/`responding`).
    Phase(Phase),
}

impl TaskLiveness {
    /// The card's view of one run's neutral activity (ADR-0105, spec #1057's
    /// P4). The token comes from the call's `(kind, detail)` via
    /// [`vocab::tool_token`]; a phase with no live call reads
    /// [`phase_token`]. A wait set renders both waits through the header's own
    /// vocabulary.
    pub(crate) fn from_run(activity: &RunActivity) -> Self {
        let activity_kind = match &activity.call {
            Some(call) => ChildActivity::Tool {
                name: vocab::tool_token(&call.identity, call.kind, &call.detail).to_string(),
                // The call's own server start is the clock: `shell 28s` counts
                // from the call, not from the last activity.
                started_at: call.started_at,
            },
            None => ChildActivity::Phase(activity.phase.unwrap_or(Phase::Waiting)),
        };
        let wait = activity.awaiting.and_then(awaiting_action);
        Self {
            activity: activity_kind,
            last_activity_ms: activity.clock_ms(),
            wait,
        }
    }

    /// The fragment's parts, spelled once (ADR-0054, spec #501): the
    /// activity's display label (a tool's own name, or the word for a phase
    /// with no live tool), the clock its age counts from (`None` when the
    /// fragment renders no age), and the wait's words. [`Self::title_fragment`]
    /// assembles these for the card; the ledger's size reserve measures these,
    /// so the two cannot drift apart over how an activity is displayed.
    pub(crate) fn title_parts(&self) -> TitleParts<'_> {
        TitleParts {
            label: match &self.activity {
                ChildActivity::Tool { name, .. } => name,
                ChildActivity::Phase(phase) => phase_token(*phase),
            },
            age_clock_ms: match &self.activity {
                ChildActivity::Tool { started_at, .. } => *started_at,
                ChildActivity::Phase(_) => self.last_activity_ms,
            },
            wait: self.wait.and_then(|wait| wait.label()),
        }
    }

    /// The title fragment a live task panel appends, in the header's own
    /// shape — the activity first, then how long it has run: `bash 28s`,
    /// `reasoning 12s`, `bash · 等待你的授权`. `now_ms` is passed in so the
    /// elapsed time is measured at card build time.
    pub fn title_fragment(&self, now_ms: i64) -> String {
        let parts = self.title_parts();
        let mut segments = match parts.age_clock_ms {
            Some(at) => vec![format!("{} {}", parts.label, fmt_elapsed(secs_since(at, now_ms)))],
            None => vec![parts.label.to_string()],
        };
        if let Some(wait) = parts.wait {
            segments.push(wait.to_string());
        }
        segments.join(" · ")
    }

    /// The epoch-ms clock the fragment's rendered age counts from — the live
    /// tool's server start, or the phase's newest activity. `None` when the
    /// fragment shows no age at all (a tool with no server clock yet), so a
    /// reader keyed on this fragment (the ledger's render clock, ADR-0060)
    /// knows there is no number to tick.
    pub fn age_clock_ms(&self) -> Option<i64> {
        self.title_parts().age_clock_ms
    }
}

/// A liveness fragment's own parts, the shape [`TaskLiveness::title_fragment`]
/// renders and the ledger's `activity_estimate` reserves room for — one
/// description, so the display and its cost cannot disagree.
pub(crate) struct TitleParts<'a> {
    /// The activity's display label: a tool's own name, or the phase word.
    /// Raw here; the ledger escapes it where it reaches a markdown body.
    pub(crate) label: &'a str,
    /// The clock the rendered age counts from; `None` when the fragment shows
    /// no age at all.
    pub(crate) age_clock_ms: Option<i64>,
    /// The wait's words without the header's icon, when one is pending.
    pub(crate) wait: Option<&'a str>,
}

/// The liveness fragment's wait vocabulary, from the run's own set: both waits
/// can be live at once (spec #1057's `RunWaits`).
fn awaiting_action(waits: RunWaits) -> Option<AwaitingAction> {
    if !waits.any() {
        return None;
    }
    match (waits.permission, waits.question) {
        (false, false) => None,
        (true, false) => Some(AwaitingAction::Permission),
        (false, true) => Some(AwaitingAction::Question),
        (true, true) => Some(AwaitingAction::Both),
    }
}

/// Seconds between two epoch-ms clocks, never negative.
fn secs_since(at_ms: i64, now_ms: i64) -> u64 {
    ((now_ms - at_ms).max(0) / 1000) as u64
}
