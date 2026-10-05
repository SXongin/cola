//! The one ending vocabulary: what ended a Turn's card, and the outcome that
//! ending stamps (spec #538, #539).
//!
//! A [`Disposition`] is decided once — by the shared settle decision
//! ([`TurnSettle`]) or by a caller that knows a loop-only ending (the sticky
//! `/stop` marker, the out-of-turn loop's two graces) — and every ending path
//! with a live card applies it through one application
//! ([`super::Turn::apply_disposition`] /
//! [`super::state::StreamAccumulator::apply_ending`]): the card's state and
//! failure line, its phase timer, then the work-context refresh and the flush.
//! The durable reap has no live accumulator to apply to: it reads the same
//! table for its ending card's state and failure line and keeps its own
//! body-preserving PATCH and record mechanics (ADR-0063).
//! This table is the ONE place an ending is translated into what the card
//! shows: the out-of-turn settle loop's private ending vocabulary and its
//! per-ending finalization dispatch retired with #539, the in-Turn drain's
//! private ending vocabulary retired with #541, and every ending path that
//! still stamps a card state directly migrates onto the same table.
//!
//! The variants are exactly the endings the Turn layer can reach:
//!
//! - [`Observe`](Disposition::Observe): the ending is not decided (a Wake's
//!   Execution has not reached its boundary yet). The caller keeps observing
//!   and stamps nothing.
//! - [`Waiting`](Disposition::Waiting): the Execution idled with live
//!   Background Tasks (ADR-0059). Not a terminal and never ✅; no Completion
//!   Notice — the next Wake continues the chain on a new card.
//! - [`Unreceived`](Disposition::Unreceived): the submitted message never
//!   reached the transcript and the Session is not live (ADR-0062). Terminal,
//!   never ✅, and silent.
//! - [`Done`](Disposition::Done): the true end, idle with no live Background
//!   Task (✅).
//! - [`Failed`](Disposition::Failed): the Turn's settled failure (ADR-0056),
//!   ❌ with the server's message.
//! - [`Stopped`](Disposition::Stopped): the deliberate `/stop` terminal (#394),
//!   ⏹ with no failure text.
//! - [`LostContact`](Disposition::LostContact) /
//!   [`StuckPanel`](Disposition::StuckPanel): the out-of-turn loop's two
//!   graces — a Backend cola cannot see, and a readable, settled card still
//!   carrying a live `⏳` panel. Both end ❌ with fixed copy.
//!
//! [`Stopped`](Disposition::Stopped), [`LostContact`](Disposition::LostContact)
//! and [`StuckPanel`](Disposition::StuckPanel) are constructed by the callers
//! that know them (the sticky stop marker; the loop's two graces) — they are
//! not settle outcomes. Everything else comes from [`TurnSettle`].

use crate::backend::TurnSettle;
use crate::feishu::card::CardState;

/// What a loop records when its reads stopped answering for the grace: the
/// card never sits on an eternal spinner over a Backend it cannot see.
pub(super) const LOST_CONTACT_ERROR: &str = "与运行失去联系，已停止更新。";

/// What a loop records when a readable, settled card still carries a live Tool
/// Panel past the grace (a crash-orphaned call): the card ends Error, never
/// Done over a `⏳` panel.
pub(super) const STUCK_PANEL_ERROR: &str = "运行已结束但工具状态未收尾，已停止更新。";

/// The Completion Notice's copy for the true end.
const NOTICE_DONE: &str = "✅ 已完成。";

/// The Completion Notice's copy for the deliberate stop.
const NOTICE_STOPPED: &str = "⏹ 已停止。";

/// The Completion Notice's copy for every ending the card records as a
/// failure (the settled failure and the loop's two fixed-copy graces).
const NOTICE_ERROR: &str = "❌ 上一条请求处理出错了，可点击卡片上的「重试」。";

/// How one ending ends the card: the state it wears, the failure line it
/// records, and whether the Completion Notice may announce it.
///
/// The one ending vocabulary every path reads, so a new ending — or a changed
/// failure copy, notice rule or classification — is one edit here and the
/// compiler forces every mapping. See the module docs for the variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// The ending is not decided: a Wake's Execution has not reached its
    /// boundary yet. The caller keeps observing; nothing is stamped.
    Observe,
    /// The Execution ended but Background Tasks are still live: the waiting
    /// yield (ADR-0059).
    Waiting,
    /// The submitted message never landed at an idle session (ADR-0062):
    /// terminal, never ✅, silent.
    Unreceived,
    /// The true end: idle with no live Background Task.
    Done,
    /// The Turn's settled failure, with the server's message (ADR-0056).
    Failed(String),
    /// `/stop` marked the session: the deliberate stop's terminal (#394).
    Stopped,
    /// No full read pair answered for the grace: a wedged Backend.
    LostContact,
    /// A readable, settled card still carried a `⏳` panel past the grace.
    StuckPanel,
}

impl From<TurnSettle> for Disposition {
    /// The one mapping from the shared settle decision (ADR-0059, ADR-0062):
    /// `Running` is the undecided read and stays [`Disposition::Observe`] —
    /// the caller keeps observing and stamps nothing.
    fn from(settle: TurnSettle) -> Self {
        match settle {
            TurnSettle::Running => Self::Observe,
            TurnSettle::Failed(error) => Self::Failed(error),
            TurnSettle::Waiting => Self::Waiting,
            TurnSettle::Unreceived => Self::Unreceived,
            TurnSettle::Complete => Self::Done,
        }
    }
}

impl Disposition {
    /// The card state this ending renders; `None` for [`Self::Observe`],
    /// which is no ending at all.
    pub(crate) fn card_state(&self) -> Option<CardState> {
        match self {
            Self::Observe => None,
            Self::Waiting => Some(CardState::Waiting),
            Self::Unreceived => Some(CardState::Unreceived),
            Self::Done => Some(CardState::Done),
            Self::Failed(_) | Self::LostContact | Self::StuckPanel => Some(CardState::Error),
            Self::Stopped => Some(CardState::Stopped),
        }
    }

    /// The failure line this ending records on the card — the server's message
    /// for a settled failure, the fixed copy for the loop's two graces — and
    /// `None` for every ending that carries none.
    pub(crate) fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(message) => Some(message),
            Self::LostContact => Some(LOST_CONTACT_ERROR),
            Self::StuckPanel => Some(STUCK_PANEL_ERROR),
            Self::Observe | Self::Waiting | Self::Unreceived | Self::Done | Self::Stopped => None,
        }
    }

    /// Whether this ending discards a failure the card had already recorded:
    /// a deliberate stop is not a failure (#394) and an Unreceived card never
    /// was one (ADR-0062). A failure ending records its own line instead, and
    /// `Done`/`Waiting` leave the recorded line exactly as it was.
    pub(crate) fn clears_error(&self) -> bool {
        matches!(self, Self::Stopped | Self::Unreceived)
    }

    /// The Completion Notice's copy when this ending is a true end the notice
    /// may announce; `None` keeps the notice silent — a Waiting yield has not
    /// reached its true end (ADR-0059) and an Unreceived ending is not a
    /// failure to report (ADR-0062). The copies are verbatim, one per ending.
    pub(crate) fn notice_copy(&self) -> Option<&'static str> {
        match self {
            Self::Done => Some(NOTICE_DONE),
            Self::Stopped => Some(NOTICE_STOPPED),
            Self::Failed(_) | Self::LostContact | Self::StuckPanel => Some(NOTICE_ERROR),
            Self::Observe | Self::Waiting | Self::Unreceived => None,
        }
    }

    /// The one line the out-of-turn loop logs for a disposition; the caller's
    /// announcement (if any) follows the apply. `Observe` never reaches the
    /// loop — it keeps observing instead of ending — but the arm stays so the
    /// match is exhaustive.
    pub(crate) fn log_line(&self) -> &'static str {
        match self {
            Disposition::Observe => "still observing",
            Disposition::Stopped => "stopped; finalized Stopped",
            Disposition::Waiting => "idle with live background tasks; yielded waiting",
            Disposition::Failed(_) => "failed; finalized Error",
            Disposition::Done => "idle; finalized",
            Disposition::Unreceived => "message never landed at idle; finalized Unreceived",
            Disposition::LostContact => "lost contact; finalized Error",
            Disposition::StuckPanel => "ended with an unreconcilable panel; finalized Error",
        }
    }

    /// The quiet true end's ending (#540, ADR-0060): Session Sync settles a
    /// yielded card in place from its own settle read plus the sticky `/stop`
    /// marker — a deliberate stop dominates a simultaneous Complete or Failed
    /// read, exactly as the out-of-turn loop's own stop check does. `None` when
    /// the read is not this path's ending: a `Running` read keeps observing,
    /// and a `Waiting` or `Unreceived` outcome leaves the card exactly where
    /// the yield left it (a Waiting card always carries an anchor, so the
    /// anchorless Unreceived decision cannot arise here).
    pub(crate) fn of_quiet_settle(settle: TurnSettle, stopped: bool) -> Option<Self> {
        match (settle, stopped) {
            (TurnSettle::Complete | TurnSettle::Failed(_), true) => Some(Self::Stopped),
            (TurnSettle::Complete, false) => Some(Self::Done),
            (TurnSettle::Failed(error), false) => Some(Self::Failed(error)),
            (TurnSettle::Waiting | TurnSettle::Running | TurnSettle::Unreceived, _) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `TurnSettle` maps to exactly one disposition; the undecided read
    /// stays `Observe` (and so stamps nothing).
    #[test]
    fn every_settle_outcome_maps_to_one_disposition() {
        assert_eq!(Disposition::from(TurnSettle::Running), Disposition::Observe);
        assert_eq!(
            Disposition::from(TurnSettle::Failed("503".into())),
            Disposition::Failed("503".into())
        );
        assert_eq!(Disposition::from(TurnSettle::Waiting), Disposition::Waiting);
        assert_eq!(Disposition::from(TurnSettle::Unreceived), Disposition::Unreceived);
        assert_eq!(Disposition::from(TurnSettle::Complete), Disposition::Done);
    }

    /// The quiet true end's mapping (#540): a `/stop` marker dominates a
    /// simultaneous Complete or Failed read — the deliberate stop is the
    /// ending, never the settled run's outcome — while a read that is not this
    /// path's ending leaves the card exactly where the yield left it.
    #[test]
    fn the_quiet_settle_maps_the_stop_marker_over_the_settle_outcome() {
        assert_eq!(
            Disposition::of_quiet_settle(TurnSettle::Complete, true),
            Some(Disposition::Stopped)
        );
        assert_eq!(
            Disposition::of_quiet_settle(TurnSettle::Failed("503".into()), true),
            Some(Disposition::Stopped)
        );
        assert_eq!(
            Disposition::of_quiet_settle(TurnSettle::Complete, false),
            Some(Disposition::Done)
        );
        assert_eq!(
            Disposition::of_quiet_settle(TurnSettle::Failed("503".into()), false),
            Some(Disposition::Failed("503".into()))
        );
        for settle in [TurnSettle::Running, TurnSettle::Waiting, TurnSettle::Unreceived] {
            for stopped in [false, true] {
                assert_eq!(
                    Disposition::of_quiet_settle(settle.clone(), stopped),
                    None,
                    "{settle:?} with stopped={stopped} is not the quiet true end"
                );
            }
        }
    }

    /// Every disposition, walked as one list: its card state, its failure line
    /// (`None` for endings that carry none) and whether it clears a recorded
    /// failure. `Observe` has no card state at all — it is no ending.
    #[test]
    fn every_disposition_maps_to_its_card_outcome() {
        let cases: &[(Disposition, Option<CardState>, Option<&str>, bool)] = &[
            (Disposition::Observe, None, None, false),
            (Disposition::Waiting, Some(CardState::Waiting), None, false),
            (Disposition::Unreceived, Some(CardState::Unreceived), None, true),
            (Disposition::Done, Some(CardState::Done), None, false),
            (
                Disposition::Failed("503".into()),
                Some(CardState::Error),
                Some("503"),
                false,
            ),
            (Disposition::Stopped, Some(CardState::Stopped), None, true),
            (
                Disposition::LostContact,
                Some(CardState::Error),
                Some(LOST_CONTACT_ERROR),
                false,
            ),
            (
                Disposition::StuckPanel,
                Some(CardState::Error),
                Some(STUCK_PANEL_ERROR),
                false,
            ),
        ];
        for (disposition, state, failure, clears_error) in cases {
            assert_eq!(
                disposition.card_state().as_ref(),
                state.as_ref(),
                "{disposition:?} card state"
            );
            assert_eq!(disposition.failure(), *failure, "{disposition:?} failure line");
            assert_eq!(
                disposition.clears_error(),
                *clears_error,
                "{disposition:?} failure clearing"
            );
            // The two failure rules never both apply: an ending that records a
            // line cannot also be clearing one.
            assert!(
                !(disposition.failure().is_some() && disposition.clears_error()),
                "{disposition:?} both records and clears a failure"
            );
        }
    }

    /// The loop's two graces carry fixed copy — pinned so a message change is
    /// one edit here, not one per loop.
    #[test]
    fn the_fixed_failure_copy_is_pinned() {
        assert_eq!(LOST_CONTACT_ERROR, "与运行失去联系，已停止更新。");
        assert_eq!(STUCK_PANEL_ERROR, "运行已结束但工具状态未收尾，已停止更新。");
        assert_eq!(Disposition::LostContact.failure(), Some(LOST_CONTACT_ERROR));
        assert_eq!(Disposition::StuckPanel.failure(), Some(STUCK_PANEL_ERROR));
    }

    /// The Completion Notice's classification, every disposition: ✅ for the
    /// true end, ⏹ for the deliberate stop, the ❌ line for every failure
    /// ending, and silence for the waiting yield and the Unreceived ending.
    #[test]
    fn the_notice_classification_is_pinned_per_disposition() {
        assert_eq!(Disposition::Done.notice_copy(), Some("✅ 已完成。"));
        assert_eq!(Disposition::Stopped.notice_copy(), Some("⏹ 已停止。"));
        assert_eq!(
            Disposition::Failed("503".into()).notice_copy(),
            Some("❌ 上一条请求处理出错了，可点击卡片上的「重试」。")
        );
        assert_eq!(
            Disposition::LostContact.notice_copy(),
            Some("❌ 上一条请求处理出错了，可点击卡片上的「重试」。")
        );
        assert_eq!(
            Disposition::StuckPanel.notice_copy(),
            Some("❌ 上一条请求处理出错了，可点击卡片上的「重试」。")
        );
        for silent in [
            Disposition::Observe,
            Disposition::Waiting,
            Disposition::Unreceived,
        ] {
            assert_eq!(silent.notice_copy(), None, "{silent:?} must stay silent");
        }
    }

    /// The loop's one log line, every disposition pinned as a table: a copy
    /// change is a deliberate edit here, visible in the operator's logs.
    #[test]
    fn the_loop_log_line_is_pinned_per_disposition() {
        let cases: &[(Disposition, &str)] = &[
            (Disposition::Observe, "still observing"),
            (Disposition::Stopped, "stopped; finalized Stopped"),
            (
                Disposition::Waiting,
                "idle with live background tasks; yielded waiting",
            ),
            (Disposition::Failed("503".into()), "failed; finalized Error"),
            (Disposition::Done, "idle; finalized"),
            (
                Disposition::Unreceived,
                "message never landed at idle; finalized Unreceived",
            ),
            (Disposition::LostContact, "lost contact; finalized Error"),
            (
                Disposition::StuckPanel,
                "ended with an unreconcilable panel; finalized Error",
            ),
        ];
        for (disposition, line) in cases {
            assert_eq!(disposition.log_line(), *line, "{disposition:?} log line");
        }
    }
}
