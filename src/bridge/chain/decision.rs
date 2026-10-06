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
//!
//! The ladder's claim — a live Turn or its follow owns the session, or an
//! inbound message is about to — comes from the one card ownership verdict
//! (ADR-0070), never from a second read here. The converse is a declaration the
//! ladder's tests pin: when this process holds the Session's card, the ladder
//! never PATCHes or collects it. A `Recorded` probe — the record names this
//! process's own card — yields `Keep` / `DiscardRecord` only; a `Successor`
//! probe's collect arms target the recorded orphan, never the held successor.

use super::records::{ChainRecord, WakeMark};
use crate::backend::{SessionTranscript, TurnAnchor, TurnSettle};
use crate::bridge::turn::CursorSeed;
use crate::opencode::types::SessionStatus;

/// The disposition of one reconcile pass over a
/// [Chain Record](ChainRecord) — the reap's whole vocabulary (ADR-0063,
/// ADR-0069). One variant per observable outcome: nothing is owed (`Keep`), a
/// deciding read is missing (`NoDecision`), the record is spent
/// (`DiscardRecord`), a successor owns the session (`CollectThenRepoint` /
/// `CollectThenRelease`), the still-live CURSORLESS orphan is owed its
/// one-time stamp (`StampRestart`), the transcript decided the ending
/// (`Settle` — PATCHed in place), or a cursor-carrying record's run is
/// projected onto a successor (`Project` for an ended run — spec #561 / ticket
/// #563 — and `ProjectLive` for a followed live one, ticket #564).
///
/// `Keep` also carries the marked outcomes: an orphan whose stamp already
/// landed or was permanently refused (#522), a yielded waiting card whose
/// yield already landed, and an ending the read left undecided
/// ([`TurnSettle::Running`]).
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
    /// The still-live CURSORLESS orphan is owed its one-time restart stamp
    /// (#443) — the fallback for a record with no Rendered Cursor (spec #561,
    /// ticket #566); a cursor-carrying record is projected instead, never
    /// stamped.
    StampRestart,
    /// The transcript decided the card's ending. `TurnSettle::Running` never
    /// surfaces here — an undecided ending is `Keep`.
    Settle(TurnSettle),
    /// A record carrying a Rendered Cursor whose run ended while cola was down
    /// (or is gone): project the missed tail — and the transcript's true
    /// ending — onto a successor card, seeded from the cursor, and collect the
    /// recorded card as taken over (spec #561, ticket #563). A cursorless
    /// record, an unresolvable cursor or an Unreceived ending keeps
    /// [`Self::Settle`] — today's in-place path.
    Project {
        /// The transcript-decided ending the successor settles with.
        settle: TurnSettle,
        /// The chain's cursor, resolved against this read: the render's seed.
        /// Carried here so the apply projects exactly what the decision proved
        /// resolvable.
        seed: CursorSeed,
    },
    /// A record carrying a Rendered Cursor whose run is still live and not
    /// owned by this process: arm a successor seeded at the cursor and FOLLOW
    /// the run through the existing external-render arm (spec #561, ticket
    /// #564) — the content produced after the seed lands on the successor as
    /// it arrives, and the run settles by transcript truth. No user message is
    /// sent: the successor's create is the restart notification.
    ///
    /// A cursor-carrying record is never stamped (ticket #566): a missing
    /// transcript read, a record with no scope to arm against and a cursor
    /// this read cannot place all claim nothing ([`Self::NoDecision`]) and
    /// wait for a read the projection can seed — never a guessed replay, and
    /// never the cursorless fallback's stamp.
    ProjectLive {
        /// The chain's cursor, resolved against this read: the render's seed.
        seed: CursorSeed,
    },
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
    ///
    /// The id and `running` are the pre-collect facts the pre-split pass read,
    /// while `anchor` is re-read **after** the successor's collect (see
    /// [`RecoveryReads::needs_successor_collect`]): a takeover can arm its
    /// anchor while the collect's awaited write is in flight, and an anchor
    /// that appears in that window must re-point the record, never be read as
    /// absent and release it.
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
    /// The ownership verdict's claim (ADR-0070) as the reap reads it: a live
    /// Turn (or the follow that inherited its guard) owns the session, or an
    /// inbound message is about to — either way the card is not orphaned.
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
    /// The record carries a Rendered Cursor (spec #561). A still-live run
    /// with one is the projection's live case (ticket #564): the transcript
    /// read is warranted to resolve the seed and follow the run, and a
    /// cursorless record keeps today's one-time restart stamp.
    pub(crate) cursor: bool,
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
    /// named status is definite non-live — only the transcript decides the
    /// ending then. A live Session keeps the card — the run may still answer
    /// it — and an unknown status claims nothing, so neither reads history.
    /// The one exception is the projection's live case (spec #561, ticket
    /// #564): a cursor-carrying record's still-live run is followed from its
    /// confirmed frontier, and the seed resolves against this read. A
    /// cursorless record keeps today's stamp and reads no history.
    pub(crate) fn needs_transcript(&self) -> bool {
        match self.status {
            Some(StatusRead::Named(status)) if !status.is_live() => true,
            Some(StatusRead::Named(status)) if status.is_live() && self.cursor => true,
            _ => false,
        }
    }

    /// Whether the successor branch's collect is owed: no claim owns the
    /// record, and a successor card owns the session. The collect precedes the
    /// decision because it is an awaited card write and the successor's armed
    /// anchor cannot be trusted before it returns — a takeover can arm the
    /// anchor while the write is in flight. The caller collects and then
    /// re-reads the anchor ([`CardProbe::Successor`]), so the decision sees the
    /// post-collect fact; the id and `running` stay the pre-collect ones,
    /// exactly the facts the pass read before the split.
    pub(crate) fn needs_successor_collect(&self) -> bool {
        !self.claimed && !self.stamping && matches!(self.card, CardProbe::Successor { .. })
    }
}

/// One record's reconcile decision (ADR-0063, ADR-0069): the whole ladder as
/// a pure function of the record and the evidence the apply gathered — for a
/// successor probe, after its collect and anchor re-read
/// ([`RecoveryReads::needs_successor_collect`]).
pub(crate) fn reconcile(record: &ChainRecord, reads: &RecoveryReads<'_>) -> ChainDisposition {
    // The ownership verdict's claim (ADR-0070) owns the session: a live Turn
    // (or the follow that inherited its guard), or an inbound message about to
    // land — either way the card is not orphaned.
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
        // record); a live or yielded card keeps it. A record whose projection
        // stopped mid-chain (spec #561, review #569) keeps it too: the tail
        // past the last confirmed slice is still owed, and the next life's
        // projection resumes from the cursor.
        CardProbe::Recorded {
            terminal,
            update_pending,
        } => {
            if *terminal && !*update_pending && !record.projection_attempted {
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
                // it, or the projection follows it (spec #561, ticket #564).
                Some(StatusRead::Named(status)) if status.is_live() => {
                    // The projection's live case (spec #561, tickets #564/#566):
                    // a record carrying a Rendered Cursor is NEVER stamped — the
                    // projection supersedes the #443 fallback. A missing
                    // transcript read, a record with no scope to arm against and
                    // a cursor this read cannot place all claim nothing and wait
                    // for a read the projection can seed; skipping content on an
                    // unplaced frontier would guess, and stamping would freeze
                    // the run the projection is supposed to follow.
                    if let Some(cursor) = record.cursor.as_ref() {
                        // A create this process life already attempted is never
                        // retried (review #569): Feishu has no idempotency key
                        // (ADR-0067), so a retry could post a duplicate
                        // successor. The record keeps observing until the run
                        // ends, when the ended pass state-repairs the old card
                        // in place.
                        if record.projection_attempted {
                            return ChainDisposition::Keep;
                        }
                        let Some(TranscriptRead::Read(transcript)) = reads.transcript.as_ref() else {
                            return ChainDisposition::NoDecision;
                        };
                        let scope = record
                            .anchor()
                            .or_else(|| transcript.anchor_of_user(record.message_id.as_str()));
                        if scope.is_some()
                            && let Some(seed) = CursorSeed::resolve(transcript, cursor)
                        {
                            return ChainDisposition::ProjectLive { seed };
                        }
                        return ChainDisposition::NoDecision;
                    }
                    // The cursorless fallback: today's one-time outcome — a
                    // stamp that landed, or was permanently refused and given
                    // up for this process life (#522), is never attempted
                    // again.
                    if record.restarted_reaped || record.restart_stamp_rejected {
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
                        // The ended-while-down projection (spec #561, ticket
                        // #563): a chain whose card confirmed a render frontier
                        // projects the missed tail onto a successor. The cursor
                        // must resolve against THIS read — an unplaceable
                        // frontier settles in place rather than guess. An
                        // Unreceived ending is no content to continue, and a
                        // still-live run keeps today's stamp (the adoption/
                        // follow is ticket #564's disposition, not this one).
                        // A create this life already attempted blocks the
                        // projection too (review #569): it is never retried, and
                        // the reap state-repairs the old card in place instead.
                        settle => {
                            if !matches!(settle, TurnSettle::Unreceived)
                                && !record.projection_attempted
                                && let Some(cursor) = record.cursor.as_ref()
                                && let Some(seed) = CursorSeed::resolve(transcript, cursor)
                            {
                                return ChainDisposition::Project { settle, seed };
                            }
                            ChainDisposition::Settle(settle)
                        }
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
    /// The session has a durable Chain Record (spec #561, ticket #566): its
    /// Wake belongs to the projection — or, cursorless, to the reap's
    /// fallback — so the Fresh path owes nothing. Only a recordless post uses
    /// the Wake Watermark's announcement rule.
    pub(crate) recorded: bool,
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

impl FreshReads {
    /// Whether the durable watermark already covers the Wake — a Wake a
    /// previous cola life showed. A restart must not re-announce it (#424):
    /// the watermark is the durable half of a chain's own `announced_wakes`,
    /// so only a strictly newer Wake is owed. Equal server times read as
    /// announced — the conservative side.
    fn covered(&self) -> bool {
        let Some(wake) = &self.wake else {
            return false;
        };
        self.announced
            .as_ref()
            .is_some_and(|mark| wake.created_ms <= mark.created_ms)
    }

    /// Whether the content-diff probe is warranted: a placeable Wake the
    /// watermark does not already cover, the conversation has not moved past,
    /// and no durable record claims the chain. An already-announced, stale or
    /// recorded Wake owes nothing whatever its work renders, so the caller
    /// skips the probe (a full transcript scan) — a probe never run leaves
    /// `renders` false, which the gate maps to `Keep` anyway.
    pub(crate) fn needs_render_probe(&self) -> bool {
        !self.recorded && self.wake.is_some() && !self.covered() && !self.stale
    }
}

/// The Fresh gate (ADR-0069): whether a restart owes a Fresh Wake
/// continuation. Pure over the read's facts — the durable record, the durable
/// watermark, the newest placeable Wake, whether the conversation moved past
/// it, and whether its own work renders. A recorded chain is never a Fresh
/// post (spec #561, ticket #566): the projection (or the cursorless fallback)
/// owns it.
pub(crate) fn fresh(reads: &FreshReads) -> FreshDisposition {
    if reads.recorded {
        return FreshDisposition::Keep;
    }
    let Some(wake) = reads.wake.clone() else {
        return FreshDisposition::NoDecision;
    };
    if reads.covered() || reads.stale || !reads.renders {
        return FreshDisposition::Keep;
    }
    FreshDisposition::Announce(wake)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MessageId, TranscriptMessage};
    use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
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
            cursor: false,
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
                cursor: false,
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

    /// The divergence essay's assertion, declared (ADR-0070, spec #545): when
    /// this process holds the Session's card, the ladder never PATCHes or
    /// collects it.
    ///
    /// The probe names the held card, and every writing disposition targets
    /// `record.card_message_id` — the settle and the restart stamp as an
    /// orphan's ending, both collect arms as the recorded predecessor's
    /// takeover — so the table decides which cards are protected:
    ///
    /// - `Recorded`: the record's card IS this process's card (whether it has
    ///   always named it or a takeover re-pointed it at the successor this
    ///   process's own lifecycle owns). Every writing arm would target the
    ///   held card, so only `Keep` / `DiscardRecord` may follow.
    /// - `Successor`: the card this process's own lifecycle owns is the
    ///   successor, while the record names the predecessor it no longer holds.
    ///   The ladder's only card writes are the collect arms, and they target
    ///   the RECORDED card — never the held successor.
    #[test]
    fn a_card_this_process_holds_is_never_patched_or_collected() {
        /// The card a disposition PATCHes, when it writes one at all.
        fn written_card<'r>(record: &'r ChainRecord, disposition: &ChainDisposition) -> Option<&'r str> {
            match disposition {
                ChainDisposition::Settle(_)
                | ChainDisposition::StampRestart
                | ChainDisposition::CollectThenRepoint { .. }
                | ChainDisposition::CollectThenRelease
                // The projections collect the recorded card as taken over and
                // settle/follow a NEW successor; they arise from a `None` probe
                // only, never while this process holds the session's card.
                | ChainDisposition::Project { .. }
                | ChainDisposition::ProjectLive { .. } => Some(record.card_message_id.as_str()),
                ChainDisposition::Keep | ChainDisposition::NoDecision | ChainDisposition::DiscardRecord => {
                    None
                }
            }
        }

        let record = record();
        let anchor = TurnAnchor {
            message_id: MessageId::new("msg_cola_next"),
            created_ms: 2_000,
        };

        // The record's card is this process's card: none of the PATCHing or
        // collecting dispositions may follow.
        for terminal in [true, false] {
            for update_pending in [true, false] {
                let reads = RecoveryReads {
                    card: CardProbe::Recorded {
                        terminal,
                        update_pending,
                    },
                    ..orphan()
                };
                let disposition = reconcile(&record, &reads);
                assert!(
                    matches!(
                        disposition,
                        ChainDisposition::Keep | ChainDisposition::DiscardRecord
                    ),
                    "the ladder must not PATCH or collect the record's own card \
                     (terminal={terminal}, update_pending={update_pending}): {disposition:?}"
                );
                assert_eq!(
                    written_card(&record, &disposition),
                    None,
                    "no card write may follow for the record's own card: {disposition:?}"
                );
            }
        }

        // A successor this process's own lifecycle owns: the ladder collects
        // only the recorded orphan, never the held successor.
        for (running, anchor) in [
            (true, Some(anchor.clone())),
            (true, None),
            (false, Some(anchor)),
            (false, None),
        ] {
            let reads = RecoveryReads {
                card: CardProbe::Successor {
                    card_message_id: "om_new".into(),
                    running,
                    anchor,
                },
                ..orphan()
            };
            let disposition = reconcile(&record, &reads);
            assert!(
                matches!(
                    disposition,
                    ChainDisposition::CollectThenRepoint { .. } | ChainDisposition::CollectThenRelease
                ),
                "a successor probe may only collect the recorded orphan \
                 (running={running}): {disposition:?}"
            );
            assert_ne!(
                written_card(&record, &disposition),
                Some("om_new"),
                "the ladder must never PATCH or collect the successor this process holds \
                 (running={running}): {disposition:?}"
            );
        }
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

        // The one-time marks hold: a stamped orphan, and (#522) one whose
        // stamp was permanently refused, are both kept, never retried.
        let stamped = ChainRecord {
            restarted_reaped: true,
            ..record.clone()
        };
        assert_eq!(reconcile(&stamped, &live), ChainDisposition::Keep);
        let rejected = ChainRecord {
            restart_stamp_rejected: true,
            ..record.clone()
        };
        assert_eq!(reconcile(&rejected, &live), ChainDisposition::Keep);
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

    /// The ended-while-down projection (spec #561, ticket #563): a record
    /// carrying a cursor whose run is not live projects the missed tail onto
    /// a successor — Complete, Failed and Waiting alike — while a cursorless
    /// record, an unresolvable cursor and an Unreceived ending all keep
    /// today's in-place settle.
    #[test]
    fn a_cursor_carrying_record_projects_an_ended_run() {
        let transcript = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ]);
        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_2000"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: 2,
                prefix_digest: Some(cursor_prefix_digest("答复")),
            }),
            live_calls: Default::default(),
        };
        let with_cursor = ChainRecord {
            cursor: Some(cursor.clone()),
            ..record()
        };
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the frontier resolves");
        assert_eq!(
            reconcile(&with_cursor, &idle_transcript(transcript.clone())),
            ChainDisposition::Project {
                settle: TurnSettle::Complete,
                seed: seed.clone(),
            },
            "a placed cursor projects the run's true end"
        );

        // The failed ending projects too, carrying the server's message.
        let failed = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            TranscriptMessage {
                error: Some("503".into()),
                ..assistant(2_000, "答复。")
            },
        ]);
        assert_eq!(
            reconcile(&with_cursor, &idle_transcript(failed.clone())),
            ChainDisposition::Project {
                settle: TurnSettle::Failed("503".into()),
                seed: CursorSeed::resolve(&failed, &cursor).unwrap(),
            }
        );

        // A waiting ending projects as well, with the successor yielding.
        // (The same answer text as the cursor's prefix: the digest must match.)
        let waiting = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ])
        .with_background_tasks(vec![background_shell(1_500)]);
        assert_eq!(
            reconcile(&with_cursor, &idle_transcript(waiting.clone())),
            ChainDisposition::Project {
                settle: TurnSettle::Waiting,
                seed: CursorSeed::resolve(&waiting, &cursor).unwrap(),
            }
        );
        // A yield already PATCHed keeps observing, exactly as today.
        let yielded = ChainRecord {
            waiting_reaped: true,
            ..with_cursor.clone()
        };
        assert_eq!(
            reconcile(&yielded, &idle_transcript(waiting)),
            ChainDisposition::Keep
        );
    }

    /// The projection's fallbacks (spec #561, ticket #563): a cursorless
    /// record, a cursor this read cannot place, and an Unreceived ending all
    /// keep today's settle path.
    #[test]
    fn a_cursorless_or_unresolvable_record_settles_in_place() {
        let transcript = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ]);
        assert_eq!(
            reconcile(&record(), &idle_transcript(transcript.clone())),
            ChainDisposition::Settle(TurnSettle::Complete),
            "a cursorless record keeps the in-place ending"
        );

        // The cursor names a part this read does not carry: settle in place,
        // never skip content on a guess.
        let dangling = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_gone"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: None,
                delivered_chars: 1,
                prefix_digest: Some(cursor_prefix_digest("答")),
            }),
            live_calls: Default::default(),
        };
        let dangling_record = ChainRecord {
            cursor: Some(dangling),
            ..record()
        };
        assert_eq!(
            reconcile(&dangling_record, &idle_transcript(transcript.clone())),
            ChainDisposition::Settle(TurnSettle::Complete)
        );

        // An Unreceived ending has no transcript content to continue. The
        // record is anchorless (the submitted message never landed) and the
        // read carries no re-derivable anchor either.
        let unreceived = SessionTranscript::new(vec![user("msg_other", 500, "上一条")]);
        let any_cursor = ChainRecord::new("om_card", MessageId::new("msg_cola_anchor"), None)
            .with_directory(Some("/work".into()));
        let any_cursor = ChainRecord {
            cursor: Some(RenderedCursor::default()),
            ..any_cursor
        };
        assert_eq!(
            reconcile(&any_cursor, &idle_transcript(unreceived)),
            ChainDisposition::Settle(TurnSettle::Unreceived)
        );
    }

    /// The #564 fallback (ticket #563): a CURSORLESS live record keeps today's
    /// one-time restart stamp, while a cursor-carrying one whose warranted
    /// transcript read is missing claims nothing (ticket #566) — a transient
    /// failure never stamps a record the projection supersedes.
    #[test]
    fn a_live_record_without_a_projection_keeps_todays_stamp() {
        let with_cursor = ChainRecord {
            cursor: Some(RenderedCursor::default()),
            ..record()
        };
        let live = |cursor: bool, transcript: Option<SessionTranscript>| RecoveryReads {
            status: Some(StatusRead::Named(SessionStatus::Busy)),
            transcript: transcript.map(TranscriptRead::Read),
            cursor,
            ..orphan()
        };
        assert_eq!(
            reconcile(&record(), &live(false, None)),
            ChainDisposition::StampRestart,
            "a cursorless live record keeps today's stamp"
        );
        assert_eq!(
            reconcile(&with_cursor, &live(true, None)),
            ChainDisposition::NoDecision,
            "a live cursor record whose warranted transcript read is missing claims nothing"
        );
    }

    /// The live adoption (spec #561, ticket #564): a record carrying a Rendered
    /// Cursor whose run is still live projects AND follows — the successor is
    /// seeded at the cursor. A cursor-carrying record is never stamped (ticket
    /// #566): an unplaceable cursor, a missing scope and a missing transcript
    /// read all claim nothing and wait for a read that can project; the #443
    /// stamp is the cursorless fallback's alone.
    #[test]
    fn a_still_live_cursor_record_projects_and_is_never_stamped() {
        let transcript = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ]);
        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_2000"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: 2,
                prefix_digest: Some(cursor_prefix_digest("答复")),
            }),
            live_calls: Default::default(),
        };
        let with_cursor = ChainRecord {
            cursor: Some(cursor.clone()),
            ..record()
        };
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the frontier resolves");
        let live = |cursor: bool, transcript: Option<SessionTranscript>| RecoveryReads {
            status: Some(StatusRead::Named(SessionStatus::Busy)),
            transcript: transcript.map(TranscriptRead::Read),
            cursor,
            ..orphan()
        };
        assert_eq!(
            reconcile(&with_cursor, &live(true, Some(transcript.clone()))),
            ChainDisposition::ProjectLive { seed },
            "a live run with a placed cursor is adopted for following"
        );

        // The cursor names a part this read does not carry: nothing may be
        // skipped, and the projection — not the #443 stamp — owns a
        // cursor-carrying record, so the pass claims nothing and a later read
        // decides.
        let dangling = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_gone"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: None,
                delivered_chars: 1,
                prefix_digest: Some(cursor_prefix_digest("答")),
            }),
            live_calls: Default::default(),
        };
        let dangling_record = ChainRecord {
            cursor: Some(dangling.clone()),
            ..record()
        };
        assert_eq!(
            reconcile(&dangling_record, &live(true, Some(transcript.clone()))),
            ChainDisposition::NoDecision,
            "an unplaceable cursor is never stamped"
        );

        // A resolvable cursor whose submitted message never landed and whose
        // record captured no server time has no scope to arm a successor with:
        // claim nothing, never the stamp.
        let anchorless = ChainRecord::new("om_card", MessageId::new("msg_cola_anchor"), None)
            .with_directory(Some("/work".into()));
        let anchorless = ChainRecord {
            cursor: Some(cursor.clone()),
            ..anchorless
        };
        let other_turn =
            SessionTranscript::new(vec![user("msg_other", 500, "上一条"), assistant(2_000, "答复。")]);
        assert_eq!(
            reconcile(&anchorless, &live(true, Some(other_turn))),
            ChainDisposition::NoDecision,
            "an anchorless cursor record is never stamped"
        );

        // The same facts on a cursorless record keep today's one-time stamp —
        // its live fallback needs no scope and no cursor.
        assert_eq!(
            reconcile(&record(), &live(false, None)),
            ChainDisposition::StampRestart,
            "the cursorless fallback keeps today's stamp"
        );
        assert_eq!(
            reconcile(&record(), &live(false, Some(transcript.clone()))),
            ChainDisposition::StampRestart,
            "a readable transcript changes nothing without a cursor"
        );
    }

    /// A create this process life already attempted is single-shot (spec #561,
    /// review #569): Feishu has no idempotency key (ADR-0067), so the live case
    /// keeps observing (the ended pass later state-repairs the old card in
    /// place) and the ended case settles in place — neither re-posts.
    #[test]
    fn an_attempted_projection_is_never_retried() {
        let transcript = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, "答复。"),
        ]);
        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_2000"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: 2,
                prefix_digest: Some(cursor_prefix_digest("答复")),
            }),
            live_calls: Default::default(),
        };
        let attempted = ChainRecord {
            cursor: Some(cursor),
            projection_attempted: true,
            ..record()
        };

        let live = RecoveryReads {
            status: Some(StatusRead::Named(SessionStatus::Busy)),
            transcript: Some(TranscriptRead::Read(transcript.clone())),
            cursor: true,
            ..orphan()
        };
        assert_eq!(
            reconcile(&attempted, &live),
            ChainDisposition::Keep,
            "an attempted live adoption is never re-posted"
        );
        assert_eq!(
            reconcile(&attempted, &idle_transcript(transcript)),
            ChainDisposition::Settle(TurnSettle::Complete),
            "an attempted ended projection state-repairs the old card in place"
        );
    }

    /// A record whose projection stopped mid-chain keeps its card (spec #561,
    /// review #569): the tail past the last confirmed slice is still owed, and
    /// the next life's projection resumes from the cursor — the terminal card
    /// alone must not discard the record that still owes content.
    #[test]
    fn an_attempted_projection_keeps_a_terminal_records_card() {
        let reads = RecoveryReads {
            card: CardProbe::Recorded {
                terminal: true,
                update_pending: false,
            },
            ..orphan()
        };
        assert_eq!(
            reconcile(&record(), &reads),
            ChainDisposition::DiscardRecord,
            "an ordinary terminal card spends its record"
        );
        let attempted = ChainRecord {
            projection_attempted: true,
            ..record()
        };
        assert_eq!(
            reconcile(&attempted, &reads),
            ChainDisposition::Keep,
            "a stopped projection keeps the record for the next life"
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
        reads.cursor = true;
        assert!(
            reads.needs_transcript(),
            "a live cursor-carrying record reads history for the projection (#564)"
        );
        reads.cursor = false;
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
    /// announced at its anchor; and a durable chain record always owes nothing
    /// (spec #561, ticket #566) — the projection, or the cursorless fallback,
    /// owns a recorded chain's Wake.
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
            recorded: false,
            wake: Some(TurnAnchor {
                message_id: MessageId::new("msg_wake"),
                created_ms: 2_000,
            }),
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

        // The plan skips the content probe for any Wake that can owe nothing:
        // already announced, stale, or absent.
        assert!(reads(None, false, false).needs_render_probe());
        assert!(!reads(Some(2_000), false, false).needs_render_probe());
        assert!(!reads(Some(3_000), false, false).needs_render_probe());
        assert!(!reads(Some(1_000), true, false).needs_render_probe());
        assert!(
            !FreshReads {
                wake: None,
                ..reads(None, false, false)
            }
            .needs_render_probe()
        );
    }

    /// The Fresh gate narrows to recordless posts (spec #561, ticket #566): a
    /// session with a durable Chain Record hands its Wake to the projection —
    /// or, cursorless, to the reap's fallback — so the gate owes nothing and
    /// skips the content probe, while the same read without a record still
    /// announces.
    #[test]
    fn the_fresh_gate_yields_to_a_chain_record() {
        let anchor = TurnAnchor {
            message_id: MessageId::new("msg_wake"),
            created_ms: 2_000,
        };
        let reads = |recorded: bool| FreshReads {
            announced: None,
            recorded,
            wake: Some(anchor.clone()),
            stale: false,
            renders: true,
        };
        assert_eq!(
            fresh(&reads(true)),
            FreshDisposition::Keep,
            "a recorded chain's Wake is the projection's, never a Fresh post"
        );
        assert!(
            !reads(true).needs_render_probe(),
            "a recorded post skips the content probe"
        );
        assert_eq!(
            fresh(&reads(false)),
            FreshDisposition::Announce(anchor),
            "a recordless post keeps the Watermark gate"
        );
    }
}
