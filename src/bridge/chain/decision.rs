//! The Chain Record module's pure decisions (ADR-0069): the reap's reconcile
//! disposition, and the Fresh gate over the Wake Watermark.
//!
//! [`reconcile`] answers "what does this record owe a restart?" as one
//! [`ChainDisposition`] over a [`RecoveryReads`] snapshot: it does no I/O,
//! failures are values, and the card writes stay with the apply
//! ([`super::reconcile`]). The read plan lives here beside the decision
//! ([`RecoveryReads::needs_status`], [`RecoveryReads::needs_transcript`]), so
//! the apply makes exactly the reads the decision consumes and the whole
//! ladder's outcomes can be unit-tested without a server.
//!
//! [`fresh`] is the module's second decision entry: the Fresh path of a Wake
//! continuation asks the same question of a chain with no live card in this
//! process, sharing the [`ChainRecords`](super::ChainRecords) facts (the Wake
//! Watermark) and the `Keep` / `NoDecision` vocabulary.

use super::records::{ChainRecord, WakeMark};
use crate::backend::{SessionTranscript, TurnAnchor, TurnSettle};
use crate::opencode::types::SessionStatus;

/// The disposition of one reconcile pass over a
/// [Chain Record](ChainRecord) — the reap's whole vocabulary (ADR-0063,
/// ADR-0069). One variant per observable outcome: nothing is owed (`Keep`), a
/// deciding read is missing (`NoDecision`), the record is spent
/// (`DiscardRecord`), a successor owns the session (`CollectThenRepoint` /
/// `CollectThenRelease`), the still-live orphan is owed its one-time stamp
/// (`StampRestart`), or the transcript decided the ending (`Settle`).
///
/// `Keep` also carries the marked outcomes: an orphan whose stamp already
/// landed, a yielded waiting card whose yield already landed, and an ending
/// the read left undecided ([`TurnSettle::Running`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChainDisposition {
    /// Nothing is owed this pass: a claim owns the record, the card is this
    /// process's own, the ending is not decided yet, or the one-time outcome
    /// is already marked.
    Keep,
    /// A deciding read is missing — failed, timed out, or made no sense (an
    /// unrecognised status kind, no directory to route by). The record stays
    /// for the next tick: evidence failures never claim anything.
    NoDecision,
    /// The record's card is this process's card and it is spent: terminal,
    /// with no write still owed from the delivery outbox (ADR-0067).
    DiscardRecord,
    /// A live successor owns the session: collect the recorded orphan, then
    /// re-point the record at it.
    CollectThenRepoint {
        /// The successor's armed Turn anchor — the re-pointed record's scope.
        anchor: TurnAnchor,
    },
    /// A settled successor owns the session: collect the recorded orphan and
    /// drop the record — there is nothing left to track.
    CollectThenRelease,
    /// The still-live orphan is owed its one-time restart stamp (#443).
    StampRestart,
    /// The transcript decided the card's ending. `TurnSettle::Running` never
    /// surfaces here — an undecided ending is `Keep`.
    Settle(TurnSettle),
}

/// What this process knows about the Session's card, as one value: the probe
/// the reap's same-card and successor branches read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CardProbe {
    /// No card in this process owns the session: the restart orphan (or a
    /// chain that vanished).
    None,
    /// The record's own card is this process's card — its own lifecycle owns
    /// it.
    Recorded {
        /// The card reached a terminal state.
        terminal: bool,
        /// The delivery outbox still owes the card a write (ADR-0067): a
        /// terminal card's record stays until its ending is confirmed.
        update_pending: bool,
    },
    /// A successor card owns the session while the record still names another
    /// — the record write raced the handover, or a path attached the new id
    /// without tracking.
    Successor {
        /// The successor's card message id — where the record re-points.
        card_message_id: String,
        /// The successor is live or yielded (non-terminal): it can carry the
        /// record.
        running: bool,
        /// The successor's armed Turn anchor, when it has one: an anchorless
        /// successor cannot scope a reap.
        anchor: Option<TurnAnchor>,
    },
}

/// The Session's status as the reap read it (ADR-0028). Failures, timeouts
/// and unrecognised kinds are all `NoEvidence`: unknown is not idle, and
/// nothing is claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusRead {
    /// The server named a status.
    Named(SessionStatus),
    /// The read failed, timed out, or named a kind this build cannot
    /// interpret.
    NoEvidence,
}

/// The Session's history as the reap read it.
#[derive(Debug, Clone)]
pub(crate) enum TranscriptRead {
    /// The server answered the read.
    Read(SessionTranscript),
    /// The read failed or timed out; the apply logged it.
    NoEvidence,
}

/// Where an orphan's reads route and what the move verdict compares against
/// (#433, #439): the directory resolution the apply performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Route<'a> {
    /// The directory the status and transcript reads route by: the record's
    /// tracked directory when it has one, else the caller's mapped fallback.
    pub(crate) directory: &'a str,
    /// The move verdict's baseline — where the card was tracked.
    pub(crate) baseline: &'a str,
}

/// One record's reconcile evidence, gathered by the apply in the decision's
/// own read order. The apply fills `status` iff [`Self::needs_status`] and
/// `transcript` iff [`Self::needs_transcript`]; [`reconcile`] treats a missing
/// read as [`ChainDisposition::NoDecision`], so a caller that forgets one
/// degrades to "nothing claimed", never to a wrong claim.
#[derive(Debug)]
pub(crate) struct RecoveryReads<'a> {
    /// A live Turn (or the follow that inherited its guard) owns the session,
    /// or an inbound message is about to: either way the card is not orphaned.
    pub(crate) claimed: bool,
    /// A restart-stamp attempt is in flight for this record (#443): its write
    /// is never cancelled and may still land, ordered by the card's delivery
    /// lock, so every other decision waits for it to resolve.
    pub(crate) stamping: bool,
    /// This process's card, or none.
    pub(crate) card: CardProbe,
    /// The route for the Session's reads, when one exists.
    pub(crate) route: Option<Route<'a>>,
    /// The status read; `None` when the plan did not warrant it.
    pub(crate) status: Option<StatusRead>,
    /// The transcript read; `None` when the plan did not warrant it.
    pub(crate) transcript: Option<TranscriptRead>,
}

impl RecoveryReads<'_> {
    /// Whether the status read is warranted: no claim owns the record, no card
    /// in this process owns the session, and a route exists to read by. With
    /// neither a tracked nor a mapped directory the pass decides nothing — on
    /// a generation whose reads are per-directory (V1) a cwd-routed status
    /// could belong to another instance's run, and stamping over a live run is
    /// worse than leaving a record for a later life (growth is bounded by the
    /// live-card sessions).
    pub(crate) fn needs_status(&self) -> bool {
        !self.claimed && !self.stamping && matches!(self.card, CardProbe::None) && self.route.is_some()
    }

    /// Whether the transcript read is warranted: the status read ran and its
    /// named status is definite non-live. A live Session keeps the card — the
    /// run may still answer it — and an unknown status claims nothing, so
    /// neither reads history.
    pub(crate) fn needs_transcript(&self) -> bool {
        matches!(self.status, Some(StatusRead::Named(status)) if !status.is_live())
    }
}

/// One record's reconcile decision (ADR-0063, ADR-0069): the whole ladder as
/// a pure function of the record and the evidence the apply gathered.
pub(crate) fn reconcile(record: &ChainRecord, reads: &RecoveryReads<'_>) -> ChainDisposition {
    // A live Turn (or the follow that inherited its guard) owns the session,
    // and an inbound message is about to: either way the card is not orphaned.
    if reads.claimed {
        return ChainDisposition::Keep;
    }
    // A restart-stamp attempt is in flight (#443): its never-cancelled write
    // may still land, ordered by the card's delivery lock, so any decision
    // here — a settle, a collect after a takeover — could be overtaken by it.
    if reads.stamping {
        return ChainDisposition::Keep;
    }
    match &reads.card {
        // The recorded card IS this process's card: its own lifecycle owns it.
        // A terminal card's record is spent once its ending write is confirmed
        // (ADR-0063 amendment — a write still pending in the outbox keeps the
        // record); a live or yielded card keeps it.
        CardProbe::Recorded {
            terminal,
            update_pending,
        } => {
            if *terminal && !*update_pending {
                ChainDisposition::DiscardRecord
            } else {
                ChainDisposition::Keep
            }
        }
        // A successor card owns the session while the record still names
        // another card: collect the recorded card in place (ADR-0063's goal:
        // never leave a card looking live), then name the live successor; a
        // settled successor keeps no record — and an anchorless one cannot
        // scope a reap.
        CardProbe::Successor { running, anchor, .. } => match (running, anchor) {
            (true, Some(anchor)) => ChainDisposition::CollectThenRepoint {
                anchor: anchor.clone(),
            },
            _ => ChainDisposition::CollectThenRelease,
        },
        // No card in this process owns the session: the restart orphan (or a
        // chain that vanished). The Session's own reads decide, and a read
        // the reap could not make claims nothing.
        CardProbe::None => {
            if reads.route.is_none() {
                return ChainDisposition::NoDecision;
            }
            match reads.status {
                // A live Session keeps the card: the run may still answer it.
                // This process holds no card for the session, so the record is
                // a restart orphan — its card froze when the previous process
                // died and nothing will move it until transcript truth ends
                // it. Stamp that once per process life (#443) so the user
                // knows why it stopped moving.
                Some(StatusRead::Named(status)) if status.is_live() => {
                    if record.restarted_reaped {
                        ChainDisposition::Keep
                    } else {
                        ChainDisposition::StampRestart
                    }
                }
                // A definite non-live status: only the transcript decides the
                // ending.
                Some(StatusRead::Named(_)) => {
                    let Some(TranscriptRead::Read(transcript)) = reads.transcript.as_ref() else {
                        return ChainDisposition::NoDecision;
                    };
                    // The settle decision's scope: the recorded anchor, else
                    // the submitted message's own anchor re-derived from this
                    // read (it may have landed after the record's last write).
                    // Absent, the message never landed — the anchorless
                    // Unreceived scope.
                    let scope = record
                        .anchor()
                        .or_else(|| transcript.anchor_of_user(record.message_id.as_str()));
                    match transcript.settle(scope.as_ref()) {
                        // The read's boundary rule is unsatisfied (a Wake's
                        // Execution has not closed): the ending is not decided
                        // — keep observing.
                        TurnSettle::Running => ChainDisposition::Keep,
                        // The wait is still on: the card yields 「⏳ 等待后台
                        // 任务」 and keeps its record, so a later read settles
                        // the true end. The ending is PATCHed once per life —
                        // a record that already yielded keeps observing.
                        TurnSettle::Waiting if record.waiting_reaped => ChainDisposition::Keep,
                        settle => ChainDisposition::Settle(settle),
                    }
                }
                // The status read was unknown, failed, or not warranted.
                _ => ChainDisposition::NoDecision,
            }
        }
    }
}

/// The Fresh gate's reads (ADR-0069's second decision entry): the facts the
/// Wake continuation's no-chain path decides on, as values.
pub(crate) struct FreshReads {
    /// The durable Wake Watermark for the session (ADR-0061): the newest Wake
    /// a previous cola life already showed.
    pub(crate) announced: Option<WakeMark>,
    /// The newest placeable Wake's anchor — the newest Wake whose server time
    /// the read carried. `None` when the read has no Wake to decide on.
    pub(crate) wake: Option<TurnAnchor>,
    /// The conversation moved past the Wake: a later user message means a
    /// later cola life already saw or superseded it, and re-posting would
    /// replay every turn that followed (the live 102k-char replay). The
    /// genuine restart case (the Wake's run is still pending, or it finished
    /// while cola was down) has no newer user message.
    pub(crate) stale: bool,
    /// The Wake's own work renders against an empty accumulator — the content
    /// diff the Fresh card is scoped by. `false` means the read owes nothing.
    pub(crate) renders: bool,
}

/// What a chain with no live card in this process is owed (ADR-0069's second
/// decision entry). Mirrors [`ChainDisposition`]'s `Keep` / `NoDecision`
/// spine; the one positive outcome is the Fresh continuation itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FreshDisposition {
    /// The restart owes a Fresh Wake continuation, scoped at this Wake anchor.
    /// The caller still owns the inbound claim and the anchor re-read that
    /// corroborate it before a card is sent.
    Announce(TurnAnchor),
    /// Nothing is owed: the Wake is already announced (the watermark is
    /// monotonic), the conversation moved past it, or its work renders
    /// nothing.
    Keep,
    /// The read carried no placeable Wake to decide on.
    NoDecision,
}

/// The Fresh gate (ADR-0069): whether a restart owes a Fresh Wake
/// continuation. Pure over the read's facts — the durable watermark, the
/// newest placeable Wake, whether the conversation moved past it, and whether
/// its own work renders.
pub(crate) fn fresh(reads: &FreshReads) -> FreshDisposition {
    let Some(wake) = reads.wake.clone() else {
        return FreshDisposition::NoDecision;
    };
    // A restart must not re-announce a Wake a previous cola life already
    // showed (#424): the watermark is the durable half of a chain's own
    // `announced_wakes`, so only a strictly newer Wake is owed. Equal server
    // times read as announced — the conservative side.
    if reads
        .announced
        .as_ref()
        .is_some_and(|mark| wake.created_ms <= mark.created_ms)
    {
        return FreshDisposition::Keep;
    }
    if reads.stale || !reads.renders {
        return FreshDisposition::Keep;
    }
    FreshDisposition::Announce(wake)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MessageId, TranscriptMessage};
    use crate::bridge::test_support::{background_shell, shell_wake};
    use crate::bridge::tests::drain::{assistant, user};

    fn record() -> ChainRecord {
        ChainRecord::new("om_card", MessageId::new("msg_cola_anchor"), Some(1_000))
            .with_directory(Some("/work".into()))
    }

    fn route() -> Route<'static> {
        Route {
            directory: "/work",
            baseline: "/work",
        }
    }

    /// Orphan reads with no server evidence yet: the caller has not run the
    /// plan.
    fn orphan() -> RecoveryReads<'static> {
        RecoveryReads {
            claimed: false,
            stamping: false,
            card: CardProbe::None,
            route: Some(route()),
            status: None,
            transcript: None,
        }
    }

    fn idle_transcript(transcript: SessionTranscript) -> RecoveryReads<'static> {
        RecoveryReads {
            status: Some(StatusRead::Named(SessionStatus::Idle)),
            transcript: Some(TranscriptRead::Read(transcript)),
            ..orphan()
        }
    }

    /// A claim owns the record before anything else is read: neither the
    /// in-process card nor any server evidence changes the decision.
    #[test]
    fn a_claim_keeps_the_record_before_anything_else() {
        let record = record();
        for reads in [
            RecoveryReads {
                claimed: true,
                card: CardProbe::Recorded {
                    terminal: true,
                    update_pending: false,
                },
                route: None,
                status: Some(StatusRead::Named(SessionStatus::Idle)),
                transcript: None,
                stamping: false,
            },
            RecoveryReads {
                stamping: true,
                ..orphan()
            },
        ] {
            assert_eq!(reconcile(&record, &reads), ChainDisposition::Keep);
        }
    }

    /// The record's own card: a terminal card with a confirmed ending is
    /// spent; a live or yielded card, and a terminal card whose write is
    /// still owed, keep the record.
    #[test]
    fn a_spent_recorded_card_drops_the_record_only_once_confirmed() {
        let record = record();
        let recorded = |terminal, update_pending| RecoveryReads {
            card: CardProbe::Recorded {
                terminal,
                update_pending,
            },
            ..orphan()
        };
        assert_eq!(
            reconcile(&record, &recorded(true, false)),
            ChainDisposition::DiscardRecord
        );
        assert_eq!(
            reconcile(&record, &recorded(false, false)),
            ChainDisposition::Keep,
            "a live or yielded card keeps its record"
        );
        assert_eq!(
            reconcile(&record, &recorded(true, true)),
            ChainDisposition::Keep,
            "a terminal card whose ending is still owed keeps its record"
        );
    }

    /// A successor: a live anchored card re-points the record at it; a
    /// settled or anchorless one releases the record after the collect.
    #[test]
    fn a_successor_repoints_or_releases() {
        let record = record();
        let successor = |running, anchor| RecoveryReads {
            card: CardProbe::Successor {
                card_message_id: "om_new".into(),
                running,
                anchor,
            },
            ..orphan()
        };
        let anchor = TurnAnchor {
            message_id: MessageId::new("msg_cola_next"),
            created_ms: 2_000,
        };
        assert_eq!(
            reconcile(&record, &successor(true, Some(anchor.clone()))),
            ChainDisposition::CollectThenRepoint {
                anchor: anchor.clone()
            }
        );
        assert_eq!(
            reconcile(&record, &successor(true, None)),
            ChainDisposition::CollectThenRelease
        );
        assert_eq!(
            reconcile(&record, &successor(false, Some(anchor.clone()))),
            ChainDisposition::CollectThenRelease
        );
    }

    /// An orphan with no route or no evidence decides nothing; a live status
    /// is the one-time stamp.
    #[test]
    fn an_orphan_needs_a_route_and_a_definite_status() {
        let record = record();
        assert_eq!(
            reconcile(
                &record,
                &RecoveryReads {
                    route: None,
                    ..orphan()
                }
            ),
            ChainDisposition::NoDecision
        );
        assert_eq!(
            reconcile(&record, &orphan()),
            ChainDisposition::NoDecision,
            "a status the plan never read claims nothing"
        );
        assert_eq!(
            reconcile(
                &record,
                &RecoveryReads {
                    status: Some(StatusRead::NoEvidence),
                    ..orphan()
                }
            ),
            ChainDisposition::NoDecision,
            "a failed, timed-out or unrecognised status claims nothing"
        );

        let live = RecoveryReads {
            status: Some(StatusRead::Named(SessionStatus::Busy)),
            ..orphan()
        };
        assert_eq!(reconcile(&record, &live), ChainDisposition::StampRestart);

        // The one-time mark holds: a stamped orphan is kept, never re-stamped.
        let stamped = ChainRecord {
            restarted_reaped: true,
            ..record.clone()
        };
        assert_eq!(reconcile(&stamped, &live), ChainDisposition::Keep);
    }

    /// The transcript decides the ending at a definite non-live status: each
    /// `TurnSettle` maps to its disposition, and an undecided or already
    /// yielded one keeps observing.
    #[test]
    fn the_transcript_decides_the_ending() {
        let record = record();
        let complete = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ]);
        assert_eq!(
            reconcile(&record, &idle_transcript(complete)),
            ChainDisposition::Settle(TurnSettle::Complete)
        );

        let failed = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            TranscriptMessage {
                error: Some("503".into()),
                ..assistant(2_000, "")
            },
        ]);
        assert_eq!(
            reconcile(&record, &idle_transcript(failed)),
            ChainDisposition::Settle(TurnSettle::Failed("503".into()))
        );

        let waiting = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "跑着。"),
        ])
        .with_background_tasks(vec![background_shell(1_500)]);
        assert_eq!(
            reconcile(&record, &idle_transcript(waiting.clone())),
            ChainDisposition::Settle(TurnSettle::Waiting)
        );
        let yielded = ChainRecord {
            waiting_reaped: true,
            ..record.clone()
        };
        assert_eq!(
            reconcile(&yielded, &idle_transcript(waiting)),
            ChainDisposition::Keep,
            "a waiting yield already PATCHed keeps observing"
        );

        // An anchorless record whose submitted message never landed: the
        // read's own probe finds no user message either, so nobody will
        // answer it — Unreceived, never ✅.
        let anchorless = ChainRecord::new("om_card", MessageId::new("msg_cola_anchor"), None)
            .with_directory(Some("/work".into()));
        let unreceived = SessionTranscript::new(vec![user("msg_cola_prev", 500, "上一条")]);
        assert_eq!(
            reconcile(&anchorless, &idle_transcript(unreceived)),
            ChainDisposition::Settle(TurnSettle::Unreceived)
        );
        // The same anchorless record settles once the read carries the
        // submitted message: the scope is re-derived from the transcript.
        let rederived = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ]);
        assert_eq!(
            reconcile(&anchorless, &idle_transcript(rederived)),
            ChainDisposition::Settle(TurnSettle::Complete)
        );

        // A Wake whose Execution boundary has not closed holds the ending
        // back even at an idle read: keep observing, never a premature ✅.
        let running = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ])
        .with_wakes(vec![shell_wake(3_000)]);
        assert_eq!(
            reconcile(&record, &idle_transcript(running)),
            ChainDisposition::Keep
        );

        // A non-live status with no transcript read (the plan would have read
        // it; a direct caller may not) claims nothing.
        assert_eq!(
            reconcile(
                &record,
                &RecoveryReads {
                    status: Some(StatusRead::Named(SessionStatus::Idle)),
                    ..orphan()
                }
            ),
            ChainDisposition::NoDecision
        );
    }

    /// The boundary rule the plan promises: the status read is warranted only
    /// without a claim or a card and with a route; the transcript only after a
    /// definite non-live status.
    #[test]
    fn the_read_plan_warrants_each_read() {
        let mut reads = orphan();
        assert!(reads.needs_status());
        assert!(!reads.needs_transcript());

        reads.status = Some(StatusRead::Named(SessionStatus::Busy));
        assert!(!reads.needs_transcript(), "a live status reads no history");
        reads.status = Some(StatusRead::NoEvidence);
        assert!(!reads.needs_transcript(), "an unknown status reads no history");
        reads.status = Some(StatusRead::Named(SessionStatus::Idle));
        assert!(reads.needs_transcript());

        for blocked in [
            RecoveryReads {
                claimed: true,
                ..orphan()
            },
            RecoveryReads {
                stamping: true,
                ..orphan()
            },
            RecoveryReads {
                card: CardProbe::Recorded {
                    terminal: false,
                    update_pending: false,
                },
                ..orphan()
            },
            RecoveryReads {
                route: None,
                ..orphan()
            },
        ] {
            assert!(
                !blocked.needs_status(),
                "no server read is warranted: {blocked:?}"
            );
        }
    }

    /// The Fresh gate: no Wake decides nothing; an announced, stale or
    /// unrendering Wake owes nothing; a strictly newer rendering one is
    /// announced at its anchor.
    #[test]
    fn the_fresh_gate_announces_only_newer_rendering_wakes() {
        let anchor = TurnAnchor {
            message_id: MessageId::new("msg_wake"),
            created_ms: 2_000,
        };
        let reads = |announced: Option<i64>, stale: bool, renders: bool| FreshReads {
            announced: announced.map(|created_ms| WakeMark {
                wake_id: "msg_wake".into(),
                created_ms,
            }),
            wake: Some(anchor.clone()),
            stale,
            renders,
        };

        assert_eq!(
            fresh(&FreshReads {
                wake: None,
                ..reads(None, false, true)
            }),
            FreshDisposition::NoDecision
        );
        assert_eq!(
            fresh(&reads(Some(2_000), false, true)),
            FreshDisposition::Keep,
            "an equal-time Wake reads as already announced"
        );
        assert_eq!(fresh(&reads(Some(3_000), false, true)), FreshDisposition::Keep);
        assert_eq!(
            fresh(&reads(Some(1_000), true, true)),
            FreshDisposition::Keep,
            "a stale Wake must not replay the conversation"
        );
        assert_eq!(
            fresh(&reads(Some(1_000), false, false)),
            FreshDisposition::Keep,
            "nothing renders owes nothing"
        );
        assert_eq!(
            fresh(&reads(Some(1_000), false, true)),
            FreshDisposition::Announce(anchor.clone())
        );
        assert_eq!(
            fresh(&reads(None, false, true)),
            FreshDisposition::Announce(anchor),
            "no watermark at all announces a rendering Wake"
        );
    }
}
