//! The stable identity of one **Background Task** run (ADR-0080): the key its
//! **Task Ending**s are recorded under.

use serde::{Deserialize, Serialize};

use crate::backend::{BackgroundTask, Wake};

/// The stable identity of one **Background Task** run (ADR-0080), and the key
/// its **Task Ending**s are recorded under.
///
/// The two sides that observe a run name it differently: a read still carrying
/// the launch names the launching tool call's id, while a **Wake** arriving
/// after the launch left the read (compaction, the V2 page cap) names only the
/// shell/child id (or the job id) it reports — never an invented one. The
/// identity therefore carries EVERY id the run is known by over one stored
/// [`Self::key`], so a writer resolves against it (an exact key, an exact Wake
/// id, else the newest record naming any other id it holds) instead of minting
/// a second record for a run that already has one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunKey {
    /// The id the run's record is stored under: the launch's call id when the
    /// writer holds one, else the shell id a Wake itself reports, else its job
    /// id, else its child session.
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
    /// The job a Wake reports for the run, when known — an alias wherever it
    /// differs from the stored key, so a later writer naming the run by its job
    /// id resolves the same record.
    #[serde(default)]
    pub(crate) job_id: Option<String>,
    /// The Wake whose completion this run recorded, when known.
    #[serde(default)]
    pub(crate) wake_id: Option<String>,
}

impl RunKey {
    /// The run a read that carries the LAUNCH names (ADR-0080): the launch's
    /// own tool call id is the stored key — the one id every writer of that
    /// read agrees on — and the ids it carries ride along as aliases.
    pub(crate) fn of_launch(task: &BackgroundTask) -> Self {
        let call_id = task.tool.call_id.clone();
        Self {
            key: call_id.clone(),
            call_id: Some(call_id),
            shell_id: task.shell_id.clone(),
            child_id: task.child_id.clone(),
            job_id: None,
            wake_id: None,
        }
    }

    /// The run a **Wake** names on its own (ADR-0080): the shell id it reports,
    /// else the job it reports, else the child session — keyed by that same id.
    /// `None` when the Wake names nothing at all: the identity is never
    /// invented. Every id it reports rides as an alias, so a writer holding the
    /// job id resolves the record the shell id keyed, and the reverse.
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
            job_id: wake.job_id.clone(),
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
            .chain(self.job_id.as_deref())
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
    pub(crate) fn absorb(&mut self, other: &RunKey) {
        for (known, held) in [
            (&mut self.call_id, &other.call_id),
            (&mut self.shell_id, &other.shell_id),
            (&mut self.child_id, &other.child_id),
            (&mut self.job_id, &other.job_id),
            (&mut self.wake_id, &other.wake_id),
        ] {
            if known.is_none() {
                *known = held.clone();
            }
        }
    }
}
