//! The Chain Record (ADR-0069): one module owns the durable per-Session facts
//! about a Card Chain, in one sidecar (`chain_records.json`).
//!
//! Two sections with two lifetimes share the file:
//!
//! - **records** — the live card record (ADR-0063): which card a Session is
//!   currently streaming into and the Turn it answers, plus the Session's
//!   directory and the per-process-life reconciliation marks. One entry owns
//!   its lifecycle: [`ChainRecords::track`] creates the record when a card
//!   becomes the Session's live card and re-points it when the chain
//!   continues, [`ChainRecords::release`] removes it once that card reaches a
//!   terminal or is collected, and [`release_spent`] carries the rule that
//!   decides the terminal case — release only once the ending write is
//!   confirmed (ADR-0063 amendment, ADR-0067).
//! - **announcements** — the Wake Watermark (ADR-0061): the newest Wake whose
//!   completion a card announced, advanced only after the carrying card write
//!   lands. Monotonic per Session and never removed, so a restart cannot
//!   re-announce a Wake a previous life already showed.
//!
//! The load fails open (a missing or corrupt file reads as empty) and the
//! write is best-effort and atomic (the shared [`sidecar`] mechanics). The
//! one deviation from the sidecar convention: the file is **kept even when
//! both sections are empty**, because its presence is the one-time migration
//! marker from the pre-ADR-0069 sidecars (`live_cards.json`,
//! `wake_watermarks.json`). When it is absent, [`ChainRecords::load`] folds
//! those legacy files into the two sections once and they are never written
//! again; removing an emptied file would fall back to them and resurrect
//! state that was deliberately removed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::backend::{MessageId, TurnAnchor};
use crate::bridge::handles::CardsHandle;
use crate::bridge::sidecar;

/// The orphan gap a restart must still deliver (spec #561, review #569): the
/// message-first takeover's undelivered tail, recorded durably because the
/// chain's Rendered Cursor has already advanced past the delivered content that
/// follows it — one frontier cannot express "gap undelivered, later content
/// delivered".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingGap {
    /// The gap's own cursor: where the undelivered tail starts, inside the
    /// orphaned Turn's window — and, once a body covering part of the gap is
    /// confirmed, where the DELIVERED part ends, so a restart resumes after it.
    pub(crate) cursor: RenderedCursor,
    /// The orphaned Turn's anchor: the window the gap lives in.
    pub(crate) anchor: TurnAnchor,
    /// The user message that ENDS the gap's window — the Turn the chain moved
    /// to when the gap was recorded. Content from there on belongs to a later
    /// card the durable cursor already covers, while [`TurnAnchor`] membership
    /// has no upper end; the walk stops before this message. `None` when the
    /// successor's Turn message could not be named: a read-derived bound (the
    /// next user message after the gap's anchor) stands in.
    #[serde(default)]
    pub(crate) bound: Option<MessageId>,
}

/// The keep rule the fresh-Turn takeover's collect applied to the card it
/// replaced (ADR-0068, spec #561), in memory only: which predecessor card the
/// collect targeted and which of the takeover seed's live-set calls it
/// resolved onto the successor — the running `⏳` markers that left the card.
/// The #443 restart stamp's post-PATCH repair reads it so a stamp landing over
/// that takeover reproduces the collect's per-call strip instead of restoring
/// the tail it removed; every other takeover (the Wake and external arms)
/// records none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredecessorKeep {
    pub(crate) card_message_id: String,
    pub(crate) resolved_calls: Vec<String>,
}

/// The transcript kind of the part a [`CursorFrontier`] names: the two kinds
/// carrying renderable model content and a settled tool panel (spec #561,
/// review #569).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CursorPartKind {
    Text,
    Reasoning,
    /// A settled tool panel (spec #561, review #569): the frontier names its
    /// position so a restart never re-renders a panel the old card already
    /// showed. `CursorFrontier::delivered_chars` then belongs to the newest
    /// text/reasoning part at or before it — a tool has no extent of its own.
    Tool,
}

/// The digest a Rendered Cursor frontier stores for a part's delivered prefix
/// (spec #561, review #569): a small, derived fingerprint of the prefix
/// CONTENT — never the content itself, and never the payload journal #505
/// rejected. It lets resolution tell a grown part (same prefix) from a
/// replacement in the same slot when both parts carry no server start time
/// (V2 decodes text parts without `time.start`), where identity alone cannot.
pub(crate) fn cursor_prefix_digest(prefix: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(prefix.as_bytes());
    u64::from_be_bytes(hash[..8].try_into().expect("sha256 is 32 bytes"))
}

/// The transcript position of the newest delivered content a Card Chain has
/// confirmed delivered (spec #561): the message identity, the part's position
/// and — for a text/reasoning frontier — the delivered character extent of
/// that part. A settled tool may be the frontier too (review #569), by
/// position alone; the extent then belongs to the newest text/reasoning part
/// at or before it, so that part still renders its growth. Position and
/// identity only, plus the prefix's derived digest — never content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CursorFrontier {
    /// The assistant message that carried the part.
    pub(crate) message_id: MessageId,
    /// The part's index in its message's typed `parts` vec, so a later read
    /// resolves the same part.
    pub(crate) part_index: usize,
    pub(crate) kind: CursorPartKind,
    /// The part's server start time, when it reported one.
    #[serde(default)]
    pub(crate) started_at: Option<i64>,
    /// Unicode characters of the part's text confirmed delivered.
    pub(crate) delivered_chars: usize,
    /// The delivered prefix's [`cursor_prefix_digest`] (spec #561, review
    /// #569): derived metadata, never content. `None` on a cursor written by
    /// an older release — or by a body whose entries carried no digest —
    /// resolution then falls back (no projection) rather than guess, the same
    /// one-release seam a cursorless record uses.
    #[serde(default)]
    pub(crate) prefix_digest: Option<u64>,
}

/// The **Rendered Cursor** (spec #561): how far a Card Chain's card has
/// confirmed rendered. The frontier is the newest delivered text/reasoning
/// position; `live_calls` is the set of tool call ids whose newest delivered
/// state was `running`. Stored on the [Chain Record](ChainRecord) and
/// advanced only by a confirmed card write; `None` on the record means
/// cursorless — an older release, or a chain whose first write has not been
/// confirmed — which reads as the existing fallback behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RenderedCursor {
    #[serde(default)]
    pub(crate) frontier: Option<CursorFrontier>,
    #[serde(default)]
    pub(crate) live_calls: std::collections::BTreeSet<String>,
}

/// One Session's durable live-card facts. `card_message_id` is the Feishu
/// message the reap PATCHes; `message_id` is the Turn's own message (the
/// `msg_cola_` user message a cola Turn submitted, or the external message an
/// adopted run answers) and `created_ms` its server time, once a read carried
/// it — the two together are the [Turn anchor](TurnAnchor) the settle decision
/// is scoped with. `created_ms` stays `None` while the submitted message has
/// never been observed: the reap then probes the transcript with `message_id`,
/// and a read that still carries no such user message ends the card Unreceived
/// (ADR-0062), never ✅.
///
/// `directory` is the Session's working directory at track time: on a
/// generation whose reads route per directory (V1), the reap must ask the
/// instance the card belongs to, never the process's cwd — and an unmapped
/// Session has no mapping to fall back to (finding #438).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChainRecord {
    pub(crate) card_message_id: String,
    /// In-memory only: this chain's **Chain Generation** (spec #571, ticket
    /// #572) — a monotonic version of the tracked card identity, bumped
    /// whenever a track, a re-point or a takeover rewrites the record onto a
    /// DIFFERENT card. A decision captures it together with the snapshot it
    /// decided from, so a card write can submit with the state version it
    /// read. Never persisted: a fresh process life (and so a record loaded
    /// from disk) starts at zero, and the generation dies with the record it
    /// belongs to.
    #[serde(skip)]
    pub(crate) generation: u64,
    pub(crate) message_id: MessageId,
    #[serde(default)]
    pub(crate) created_ms: Option<i64>,
    #[serde(default)]
    pub(crate) directory: Option<String>,
    /// In-memory only: the reap already PATCHed this card into its waiting
    /// ending. A record that is rewritten (a new card becomes live) starts
    /// unmarked; the flag only suppresses PATCHing the same waiting card on
    /// every Session Sync tick, and never survives a restart.
    #[serde(skip)]
    pub(crate) waiting_reaped: bool,
    /// In-memory only: the reap already stamped this card's still-live
    /// restart orphan (#443) with the restart status. A record that is
    /// rewritten (a new card becomes live) starts unmarked; the flag only
    /// suppresses re-stamping the same orphan on every Session Sync tick, and
    /// never survives a restart — a fresh life stamps once again, which is
    /// idempotent (the stamp changes only the header).
    #[serde(skip)]
    pub(crate) restarted_reaped: bool,
    /// In-memory only: a restart-stamp attempt for this card is in flight
    /// (#443) — its view read or PATCH is running in a detached task. The
    /// claim keeps the reap from starting a second attempt, and from deciding
    /// anything (a settle, a successor collect) that the never-cancelled
    /// write could still land after; it is released when the attempt resolves
    /// and never survives a restart.
    #[serde(skip)]
    pub(crate) restart_stamping: bool,
    /// In-memory only: a restart-stamp attempt for this card was permanently
    /// refused by Feishu (`CardContentRejected`, #522) — the deterministic
    /// refusal means the preserved payload can never land — so this process
    /// life gives the stamp up instead of retrying it every Session Sync tick.
    /// The record still keeps the card: it stays frozen until its real ending
    /// supersedes the stamp, exactly the pre-#443 behavior for that rare card.
    /// A fresh life starts unmarked (the refusal was about the payload, and
    /// the stamp is idempotent), and a record rewritten for a new card starts
    /// unmarked too.
    #[serde(skip)]
    pub(crate) restart_stamp_rejected: bool,
    /// In-memory only: the keep rule the fresh-Turn takeover's collect applied
    /// to the card this record replaced (ADR-0068). The #443 restart stamp's
    /// post-PATCH repair reads it so a stamp that lands over that takeover
    /// reproduces the collect's strip — never restoring the tail the collect
    /// removed. Every other takeover leaves it `None`, and the repair then
    /// keeps today's body. A record that is rewritten (a later handover)
    /// starts `None`; it never persists.
    #[serde(skip)]
    pub(crate) predecessor_keep: Option<PredecessorKeep>,
    /// In-memory only: this chain's projection already attempted its successor
    /// create in this process life (spec #561, review #569). Feishu has no
    /// idempotency key (ADR-0067), so a create whose outcome is not a definite
    /// non-landing is never retried — it may have landed, and a retry would
    /// post a duplicate successor. The record stays for the reap's in-place
    /// state repair instead. A record rewritten for a new card starts
    /// unmarked, and the mark never survives a restart.
    #[serde(skip)]
    pub(crate) projection_attempted: bool,
    /// Durable (spec #561, review #569): a successor create is in flight for
    /// this chain. Persisted BEFORE the create — the projection's write-ahead
    /// intent — so a process that dies between the create and the takeover's
    /// re-point leaves the single-shot fact behind: the next life treats the
    /// create as ambiguous and never re-posts a duplicate, and the old card
    /// takes the state-repair path. Consumed when the projection succeeds (the
    /// takeover rewrites the record, and a fresh record starts unmarked) or
    /// cleared when a DEFINITE non-delivery proves no message was created, so a
    /// retry stays safe. Scoped to the record's generation by its own lifetime.
    #[serde(default)]
    pub(crate) projection_intent: bool,
    /// Durable (spec #561, review #569): the orphan gap this chain still owes —
    /// the message-first takeover's undelivered tail, whose start the Rendered
    /// Cursor can no longer express once the delivered content after it
    /// advanced the frontier. Rendered once by the projection that finds it (or
    /// by the fresh Turn that carries it), then consumed: a projection's
    /// re-point rewrites the record (while an unrendered gap is carried onto
    /// the new one), and a rendered gap's first confirmed write clears it.
    #[serde(default)]
    pub(crate) pending_gap: Option<PendingGap>,
    /// The Rendered Cursor (spec #561): how far this chain's card has
    /// confirmed rendered. `None` — cursorless — is an older release's record
    /// or a chain whose first confirmed write has not landed; both read as the
    /// existing fallback behavior.
    #[serde(default)]
    pub(crate) cursor: Option<RenderedCursor>,
}

impl ChainRecord {
    pub(crate) fn new(
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
    ) -> Self {
        Self {
            card_message_id: card_message_id.into(),
            generation: 0,
            message_id,
            created_ms,
            directory: None,
            waiting_reaped: false,
            restarted_reaped: false,
            restart_stamping: false,
            restart_stamp_rejected: false,
            predecessor_keep: None,
            projection_attempted: false,
            projection_intent: false,
            pending_gap: None,
            cursor: None,
        }
    }

    /// Carry the Session's directory so the reap's reads route to the right
    /// instance. An empty directory is unknown, exactly like `None`.
    pub(crate) fn with_directory(mut self, directory: Option<String>) -> Self {
        self.directory = directory.filter(|directory| !directory.is_empty());
        self
    }

    /// The Turn anchor this record scopes a settle decision with, when the
    /// message's server time was captured. `None` means the submitted message
    /// was never observed in a transcript — the anchorless Unreceived scope.
    /// The message id travels with its time as one fact, like every anchor.
    pub(crate) fn anchor(&self) -> Option<TurnAnchor> {
        Some(TurnAnchor {
            message_id: self.message_id.clone(),
            created_ms: self.created_ms?,
        })
    }
}

/// The in-memory **Chain Generation** a record write leaves behind (spec #571,
/// ticket #572): a record replacing one that named the same card carries its
/// generation (a re-track is not an identity change), a record re-pointed onto
/// a different card bumps it by one, and a genuinely new chain starts at zero.
fn chain_generation(previous: Option<&ChainRecord>, card_message_id: &str) -> u64 {
    match previous {
        Some(previous) if previous.card_message_id == card_message_id => previous.generation,
        Some(previous) => previous.generation.saturating_add(1),
        None => 0,
    }
}

/// One session's newest announced Wake: its identity and server time travel
/// together, the same one-fact anchor rule as [`TurnAnchor`].
///
/// [`TurnAnchor`]: crate::backend::TurnAnchor
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WakeMark {
    pub(crate) wake_id: String,
    pub(crate) created_ms: i64,
}

/// The persisted Chain Record; a wrapper so the on-disk shape can grow.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ChainRecordFile {
    #[serde(default)]
    records: HashMap<String, ChainRecord>,
    #[serde(default)]
    announcements: HashMap<String, WakeMark>,
}

/// The pre-ADR-0069 shape, read once when `chain_records.json` is absent and
/// never written again. The explicit `bound` keeps serde's `#[serde(default)]`
/// inference from demanding `T: Default` (only the map is defaulted).
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct LegacySessions<T> {
    #[serde(default)]
    sessions: HashMap<String, T>,
}

impl<T> Default for LegacySessions<T> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
}

/// The in-memory mirror of the Chain Record, under one lock: every mutation
/// happens and persists under it, so the two sections can never be observed
/// half-written against each other.
struct State {
    records: HashMap<String, ChainRecord>,
    announcements: HashMap<String, WakeMark>,
}

/// The write-through mirror of the Chain Record. Locked independently of the
/// cards and the session store: every reader (the card paths that track a
/// live card, the reap, the Fresh gate) reaches it through `CardsHandle`, and
/// no lock ordering against `cards.cards` matters because every write is a
/// plain synchronous file write.
pub(crate) struct ChainRecords {
    path: PathBuf,
    state: Mutex<State>,
    /// Test-only: how often the sidecar was actually persisted (spec #561's
    /// write-cost measurement). The counter exists only in test builds, so
    /// production pays nothing for the measurement.
    #[cfg(test)]
    writes: std::sync::atomic::AtomicUsize,
}

impl ChainRecords {
    /// Load the Chain Record — from `chain_records.json` when it exists, else
    /// by folding the legacy `live_cards.json` / `wake_watermarks.json` beside
    /// it (ADR-0069's one-time migration). A missing or corrupt file is an
    /// empty record, never an error: a lost record only means a restart reaps
    /// nothing and a Fresh path may re-announce once.
    pub(crate) fn load(path: PathBuf) -> Self {
        let folded = !path.exists();
        let state = if folded {
            let records = sidecar::load::<LegacySessions<ChainRecord>>(
                &path.with_file_name("live_cards.json"),
                "live-card record",
            )
            .sessions;
            let announcements = sidecar::load::<LegacySessions<WakeMark>>(
                &path.with_file_name("wake_watermarks.json"),
                "Wake watermark",
            )
            .sessions;
            State {
                records,
                announcements,
            }
        } else {
            let file = sidecar::load::<ChainRecordFile>(&path, "Chain Record");
            State {
                records: file.records,
                announcements: file.announcements,
            }
        };
        let store = Self {
            path,
            state: Mutex::new(state),
            #[cfg(test)]
            writes: std::sync::atomic::AtomicUsize::new(0),
        };
        if folded {
            // Materialize the marker now: the folded state is what the legacy
            // files held, and the first load is what settles the migration, not
            // the first later mutation. The subsequent loads never look back.
            let state = store.lock();
            store.write(&state);
        }
        store
    }

    /// The session's record, when its card is (believed to be) live. The test
    /// seam: production reads the whole record through [`Self::entries`] (the
    /// reap's pass) or the previous value [`Self::track`] returns.
    #[cfg(test)]
    pub(crate) fn get(&self, session_id: &str) -> Option<ChainRecord> {
        self.lock().records.get(session_id).cloned()
    }

    /// Every record, as one snapshot — Session Sync's reap iterates this so a
    /// record whose session is no longer mapped is still reconciled.
    pub(crate) fn entries(&self) -> Vec<(String, ChainRecord)> {
        self.lock()
            .records
            .iter()
            .map(|(session_id, card)| (session_id.clone(), card.clone()))
            .collect()
    }

    /// The chain's **current** Chain Generation (spec #571, ticket #574) — the
    /// version a card write submitted now must carry, read at submission so a
    /// takeover that landed since the caller's own snapshot (a new collect)
    /// outranks that snapshot's older generation instead of being refused by
    /// it. A released chain has no version left, and no writer of it remains;
    /// zero is what a fresh chain starts at.
    pub(crate) fn generation(&self, session_id: &str) -> u64 {
        self.lock()
            .records
            .get(session_id)
            .map_or(0, |record| record.generation)
    }

    /// The one creation/re-point entry: make the record naming
    /// `card_message_id` the session's record from the current Turn facts and
    /// the Session's directory, whether that card opens a chain, continues it
    /// or takes it over from a predecessor — so the same facts can never build
    /// two different records. Returns the previous record, the predecessor a
    /// takeover acts on. Best-effort: a failed write leaves the in-memory
    /// record correct (the next write retries the file), which at worst means
    /// this card is not reaped by the next restart.
    pub(crate) fn track(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
    ) -> Option<ChainRecord> {
        let mut state = self.lock();
        // A re-point within the chain carries the Rendered Cursor (spec #561):
        // a split continuation or a new Turn on the same chain must not clear
        // the chain's frontier — only a genuinely new chain (no previous
        // record) starts cursorless.
        let previous = state.records.get(session_id).cloned();
        let mut record = ChainRecord::new(card_message_id, message_id, created_ms)
            .with_directory(directory.map(str::to_string));
        record.generation = chain_generation(previous.as_ref(), &record.card_message_id);
        if let Some(previous) = &previous {
            record.cursor = previous.cursor.clone();
            // The owed orphan gap is chain-level too (spec #561, review #569):
            // a split continuation, a Wake continuation or a fresh Turn's
            // takeover must not drop the tail a later read still owes. The
            // projection's own re-point confirms its coverage separately —
            // consuming the fact when the body reached the gap's end.
            record.pending_gap = previous.pending_gap.clone();
            // So is the projection's write-ahead intent (spec #561, review
            // #569): a create it marked may have landed with its takeover lost
            // to this re-point, and a chain that drops the mark would let a
            // later restart post that tail a second time. The projection's own
            // take-over consumes it (a fresh record without the mark, or an
            // explicit clear on the loser's late-card collect).
            record.projection_intent = previous.projection_intent;
        }
        state.records.insert(session_id.to_string(), record);
        self.write(&state);
        previous
    }

    /// A fresh Turn's TAKEOVER (spec #561, review #569): re-point the record
    /// onto the new card AND record the owed orphan gap in ONE write — the
    /// facts are known here, before any transcript read: the previous record's
    /// cursor and anchor are the orphaned Turn's frontier and window, and the
    /// new Turn's message is the window's bound. Nothing may depend on the
    /// later read to become owed: a crash while that read is pending would
    /// otherwise leave a record that settles Unreceived and releases the
    /// orphan's undelivered tail. The previous cursor carries, the previous
    /// gap wins when there is one (its cursor may have advanced past the
    /// derived frontier), and an unresolved write-ahead intent carries too.
    pub(crate) fn track_takeover(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
    ) -> Option<ChainRecord> {
        let mut state = self.lock();
        let previous = state.records.get(session_id).cloned();
        let mut record = ChainRecord::new(card_message_id, message_id.clone(), created_ms)
            .with_directory(directory.map(str::to_string));
        record.generation = chain_generation(previous.as_ref(), &record.card_message_id);
        if let Some(previous) = &previous {
            record.cursor = previous.cursor.clone();
            record.pending_gap = previous.pending_gap.clone().or_else(|| {
                Some(PendingGap {
                    cursor: previous.cursor.clone()?,
                    anchor: previous.anchor()?,
                    bound: Some(message_id.clone()),
                })
            });
            record.projection_intent = previous.projection_intent;
        }
        state.records.insert(session_id.to_string(), record);
        self.write(&state);
        previous
    }

    /// Consume the write-ahead intent whatever card the session's record now
    /// names (spec #561, review #569): a projection that LOST its create
    /// window to a fresh Turn collects its late card here, and the winning
    /// chain — which carried the unresolved intent through its own takeover —
    /// must not keep it, or a later restart could project the late card's tail
    /// again.
    pub(crate) fn clear_projection_intent_any(&self, session_id: &str) -> bool {
        let mut state = self.lock();
        let Some(card) = state.records.get_mut(session_id) else {
            return false;
        };
        if !card.projection_intent {
            return false;
        }
        card.projection_intent = false;
        self.write(&state);
        true
    }

    /// Re-point the session's record AND set its Rendered Cursor in ONE chains
    /// critical section (spec #561, review #569): a projection's atomic
    /// takeover. A separate [`Self::track`] + [`Self::advance_cursor`] would
    /// leave the record naming the successor card with the PREDECESSOR's
    /// frontier in between, and a fresh Turn that snapshots the record in that
    /// window would seed from the stale cursor and re-render the successor's
    /// already-delivered tail onto its own card — while the collected successor
    /// keeps its body: the same text twice. Returns the previous record,
    /// exactly like [`Self::track`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn track_carrying_cursor(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        cursor: &RenderedCursor,
        gap: Option<(&CursorFrontier, bool)>,
    ) -> Option<ChainRecord> {
        let mut state = self.lock();
        let previous = state.records.get(session_id).cloned();
        let mut record = ChainRecord::new(card_message_id, message_id, created_ms)
            .with_directory(directory.map(str::to_string));
        record.generation = chain_generation(previous.as_ref(), &record.card_message_id);
        record.cursor = Some(cursor.clone());
        // The gap rides the re-point exactly as it does through [`Self::track`]
        // (spec #561, review #569), and the confirmed body's coverage settles
        // it in this SAME record write: the cursor and the gap can never be
        // observable apart — a crash between two updates would leave the
        // cursor ahead of the gap, and the recovery would re-render gap
        // content a card already showed.
        record.pending_gap = previous
            .as_ref()
            .and_then(|previous| previous.pending_gap.clone());
        match (record.pending_gap.as_mut(), gap) {
            // A body that reached the gap's end consumes it with the re-point.
            (Some(_), Some((_, true))) => record.pending_gap = None,
            // A partial body advances the gap past what the successor shows.
            (Some(recorded), Some((frontier, false))) => {
                recorded.cursor.frontier = Some(frontier.clone());
            }
            _ => {}
        }
        state.records.insert(session_id.to_string(), record);
        self.write(&state);
        previous
    }

    /// The one removal: drop the session's record — the card reached a
    /// terminal or was collected, so nothing is owed a reap. Its announcement,
    /// if any, stays: the watermark outlives the record it was announced on.
    pub(crate) fn release(&self, session_id: &str) {
        let mut state = self.lock();
        if state.records.remove(session_id).is_some() {
            self.write(&state);
        }
    }

    /// Drop `session_id`'s record only while it still names
    /// `card_message_id` (spec #561, review #569): the ending that just landed
    /// belongs to that card, and a chain that moved on meanwhile — a fresh Turn
    /// taking the session over — keeps its own record. Returns whether a
    /// record was released.
    pub(crate) fn release_if_card(&self, session_id: &str, card_message_id: &str) -> bool {
        let mut state = self.lock();
        if state
            .records
            .get(session_id)
            .is_none_or(|record| record.card_message_id != card_message_id)
        {
            return false;
        }
        state.records.remove(session_id);
        self.write(&state);
        true
    }

    /// Re-key a record when the session it names is recreated under a fresh id
    /// (the 404 recreate): the card is still live, only its session id moved.
    /// The Session's announcement moves with it — the recreated id names the
    /// same logical Session, so a Wake the old id announced must not be
    /// re-announced under the new one.
    pub(crate) fn rename(&self, from: &str, to: &str) {
        let mut state = self.lock();
        let record = state.records.remove(from);
        let announcement = state.announcements.remove(from);
        if record.is_none() && announcement.is_none() {
            return;
        }
        if let Some(record) = record {
            state.records.insert(to.to_string(), record);
        }
        if let Some(announcement) = announcement {
            state.announcements.insert(to.to_string(), announcement);
        }
        self.write(&state);
    }

    /// Refresh the anchor of the record naming `card_message_id` — the render
    /// path's write once a read carries the submitted message's server time.
    /// A no-op when the record names another card (the chain moved on, a
    /// successor owns the session) or the anchor is already current, so the
    /// frequent render polls do not rewrite the file.
    pub(crate) fn set_anchor(&self, session_id: &str, card_message_id: &str, anchor: &TurnAnchor) {
        let mut state = self.lock();
        let Some(card) = state.records.get_mut(session_id) else {
            return;
        };
        if card.card_message_id != card_message_id
            || (card.message_id == anchor.message_id && card.created_ms == Some(anchor.created_ms))
        {
            return;
        }
        card.message_id = anchor.message_id.clone();
        card.created_ms = Some(anchor.created_ms);
        self.write(&state);
    }

    /// The session's Rendered Cursor, when its record carries one (spec #561).
    /// `None` for a missing record or a cursorless one — a legacy record or a
    /// chain whose first confirmed write has not landed. The projection reads
    /// it at apply time (ticket #563): the record snapshot the reap took
    /// before its server reads can be older than a concurrent confirmed write.
    pub(crate) fn cursor(&self, session_id: &str) -> Option<RenderedCursor> {
        self.lock()
            .records
            .get(session_id)
            .and_then(|record| record.cursor.clone())
    }

    /// Whether the session has a durable Chain Record at all — the Fresh
    /// gate's one read (spec #561, ticket #566). A recorded chain's Wake
    /// belongs to the projection (or, cursorless, to the reap's fallback),
    /// so only a recordless post uses the Wake Watermark's announcement rule.
    pub(crate) fn recorded(&self, session_id: &str) -> bool {
        self.lock().records.contains_key(session_id)
    }

    /// Advance the Rendered Cursor of the record naming `card_message_id`
    /// (spec #561) with no gap riding along — the test seam and the seeding
    /// helpers' form of [`Self::advance_cursor_and_gap`].
    #[cfg(test)]
    pub(crate) fn advance_cursor(&self, session_id: &str, card_message_id: &str, cursor: &RenderedCursor) {
        self.advance_cursor_and_gap(session_id, card_message_id, cursor, None);
    }

    /// Advance the Rendered Cursor AND settle the orphan gap in ONE record
    /// write (spec #561, review #569): two separate updates would leave a
    /// crash window between them — the cursor ahead of the gap — and the
    /// recovery would re-render gap content a card already showed. `gap` is
    /// `Some((frontier, complete))` when the confirmed body carried gap
    /// content: a partial coverage advances the gap's cursor (so a restart
    /// resumes after what a card showed and repeats nothing), and a body that
    /// reached the gap's end consumes the durable fact. Best-effort and
    /// atomic, like every record write.
    pub(crate) fn advance_cursor_and_gap(
        &self,
        session_id: &str,
        card_message_id: &str,
        cursor: &RenderedCursor,
        gap: Option<(&CursorFrontier, bool)>,
    ) {
        let mut state = self.lock();
        let Some(card) = state.records.get_mut(session_id) else {
            return;
        };
        if card.card_message_id != card_message_id {
            return;
        }
        let cursor_current = card.cursor.as_ref() == Some(cursor);
        let gap_change = match (card.pending_gap.as_mut(), gap) {
            (Some(recorded), Some((frontier, complete))) => {
                if complete {
                    card.pending_gap = None;
                    true
                } else if recorded.cursor.frontier.as_ref() == Some(frontier) {
                    false
                } else {
                    recorded.cursor.frontier = Some(frontier.clone());
                    true
                }
            }
            _ => false,
        };
        if cursor_current && !gap_change {
            return;
        }
        card.cursor = Some(cursor.clone());
        self.write(&state);
    }

    /// Mark that `card_message_id`'s waiting ending was already PATCHed, so
    /// the reap does not repeat it every tick. In-memory only: a restart
    /// re-PATCHes once, which is idempotent. Returns whether the record still
    /// names that card (a stale mark for a replaced card is dropped).
    pub(crate) fn mark_waiting_reaped(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| card.waiting_reaped = true)
    }

    /// Mark that `card_message_id`'s restart stamp was already PATCHed, so the
    /// reap stamps a still-live restart orphan once per process life (#443).
    /// In-memory only: a restart re-stamps once, which is idempotent. Returns
    /// whether the record still names that card (a stale mark for a replaced
    /// card is dropped).
    pub(crate) fn mark_restarted_reaped(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| card.restarted_reaped = true)
    }

    /// Mark that `card_message_id`'s restart stamp was permanently refused by
    /// Feishu (`CardContentRejected`, #522): the same payload can never land,
    /// so the reap gives the stamp up for this process life instead of
    /// retrying it every Session Sync tick. In-memory only, like
    /// [`Self::mark_restarted_reaped`]. Returns whether the record still names
    /// that card (a stale mark for a replaced card is dropped).
    pub(crate) fn mark_restart_stamp_rejected(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| {
            card.restart_stamp_rejected = true
        })
    }

    /// Mark that `card_message_id`'s projection already attempted its successor
    /// create in this process life (spec #561, review #569): Feishu has no
    /// idempotency key (ADR-0067), so the create is single-shot — a later pass
    /// must state-repair the old card instead of re-posting a successor that
    /// may already have landed. In-memory only; a record rewritten for a new
    /// card starts unmarked. Returns whether the record still names that card.
    pub(crate) fn mark_projection_attempted(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| {
            card.projection_attempted = true
        })
    }

    /// Persist the projection's write-ahead intent BEFORE its successor create
    /// (spec #561, review #569): a process that dies between the create and the
    /// takeover's re-point leaves this durable single-shot fact behind, so the
    /// next life treats the create as ambiguous and never re-posts. Scoped to
    /// the record naming `card_message_id`: a later re-point rewrites the
    /// record, which starts unmarked.
    pub(crate) fn note_projection_intent(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_projection_intent(session_id, card_message_id, true)
    }

    /// Clear the write-ahead intent once a DEFINITE non-delivery proves the
    /// platform created no message, so a later pass or restart may retry
    /// (spec #561, review #569). Scoped to the record naming `card_message_id`,
    /// exactly like [`Self::note_projection_intent`].
    pub(crate) fn clear_projection_intent(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_projection_intent(session_id, card_message_id, false)
    }

    /// The shared body of the write-ahead intent's set/clear: apply it while
    /// the record still names `card_message_id` and persist the change. A value
    /// already current is left alone, so repeated notes never rewrite the file.
    /// Returns whether the record was the named one.
    fn set_projection_intent(&self, session_id: &str, card_message_id: &str, intent: bool) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                if card.projection_intent != intent {
                    card.projection_intent = intent;
                    self.write(&state);
                }
                true
            }
            _ => false,
        }
    }

    /// Persist the orphan gap a takeover owes (spec #561, review #569), scoped
    /// to the record naming `card_message_id`: the fresh Turn's failed seed and
    /// a carried predecessor's gap both record it here, so a restart renders
    /// exactly the gap and nothing the old cards already showed.
    pub(crate) fn note_pending_gap(&self, session_id: &str, card_message_id: &str, gap: &PendingGap) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                if card.pending_gap.as_ref() != Some(gap) {
                    card.pending_gap = Some(gap.clone());
                    self.write(&state);
                }
                true
            }
            _ => false,
        }
    }

    /// Record how far a confirmed write carried the chain's pending orphan gap
    /// (spec #561, review #569), scoped like [`Self::note_pending_gap`]: a body
    /// that stopped mid-gap ADVANCES the gap's cursor — so a restart (or the
    /// next slice) resumes after the content a card already showed instead of
    /// re-rendering it — while a body that reached the gap's end consumes the
    /// durable fact. Production confirmations go through the ONE-write
    /// [`Self::advance_cursor_and_gap`]; this is the store tests' seam.
    #[cfg(test)]
    pub(crate) fn advance_pending_gap(
        &self,
        session_id: &str,
        card_message_id: &str,
        frontier: &CursorFrontier,
        complete: bool,
    ) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                let Some(gap) = card.pending_gap.as_mut() else {
                    return false;
                };
                if complete {
                    card.pending_gap = None;
                } else {
                    gap.cursor.frontier = Some(frontier.clone());
                }
                self.write(&state);
                true
            }
            _ => false,
        }
    }

    /// Record the keep rule the fresh-Turn takeover's collect is about to
    /// apply to `card_message_id` (ADR-0068): `resolved_calls` are the live-set
    /// calls the seed resolved onto the successor, whose running panels the
    /// collect strips. Recorded before the collect's PATCH so a stamp that
    /// lands after it finds the rule. In-memory only; a later handover that
    /// rewrites the record drops it.
    pub(crate) fn note_predecessor_keep(
        &self,
        session_id: &str,
        card_message_id: &str,
        resolved_calls: &[String],
    ) {
        let mut state = self.lock();
        if let Some(card) = state.records.get_mut(session_id) {
            card.predecessor_keep = Some(PredecessorKeep {
                card_message_id: card_message_id.to_string(),
                resolved_calls: resolved_calls.to_vec(),
            });
        }
    }

    /// The running panels the takeover collect stripped from the predecessor
    /// card `card_message_id`, when this record still remembers them:
    /// `Some(resolved_calls)` after a fresh-Turn takeover's collect, `None` for
    /// every other takeover (the #443 repair then keeps today's body) and for a
    /// record that moved on.
    pub(crate) fn predecessor_keep_strip(
        &self,
        session_id: &str,
        card_message_id: &str,
    ) -> Option<Vec<String>> {
        self.lock().records.get(session_id).and_then(|card| {
            card.predecessor_keep
                .as_ref()
                .filter(|keep| keep.card_message_id == card_message_id)
                .map(|keep| keep.resolved_calls.clone())
        })
    }

    /// Set `flag` on the record naming `card_message_id` — the shared body of
    /// the record-scoped flag mutations (the reap's one-per-life marks and the
    /// restart-stamp release). Returns whether the record still names that
    /// card (a stale mutation for a replaced card is dropped).
    fn set_reap_flag(
        &self,
        session_id: &str,
        card_message_id: &str,
        flag: impl FnOnce(&mut ChainRecord),
    ) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                flag(card);
                true
            }
            _ => false,
        }
    }

    /// Claim the one in-flight restart-stamp attempt for `card_message_id`
    /// (#443): true when the caller now owns the attempt, false when the
    /// record moved on, the stamp already landed or was permanently refused
    /// (#522), or another attempt holds the claim. In-memory only.
    pub(crate) fn begin_restart_stamp(&self, session_id: &str, card_message_id: &str) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card)
                if card.card_message_id == card_message_id
                    && !card.restarted_reaped
                    && !card.restart_stamping
                    && !card.restart_stamp_rejected =>
            {
                card.restart_stamping = true;
                true
            }
            _ => false,
        }
    }

    /// Release `card_message_id`'s in-flight restart-stamp claim once its
    /// attempt resolved — landed (the record carries the mark) or failed, so
    /// the next pass may retry. A record that no longer names the card is
    /// left untouched.
    pub(crate) fn finish_restart_stamp(&self, session_id: &str, card_message_id: &str) {
        self.set_reap_flag(session_id, card_message_id, |card| {
            card.restart_stamping = false;
        });
    }

    /// The session's mark, when one exists — the Fresh path's gate: a Wake at
    /// or below it was already announced.
    pub(crate) fn announced(&self, session_id: &str) -> Option<WakeMark> {
        self.lock().announcements.get(session_id).cloned()
    }

    /// Advance the session's mark to `(wake_id, created_ms)` and persist, when
    /// strictly newer than the current one. Best-effort: a failed write leaves
    /// the in-memory mark advanced (the next advance retries the file), which
    /// suppresses at most the announcement whose write raced the crash — the
    /// same duplicate-over-loss trade the drain itself makes.
    pub(crate) fn advance(&self, session_id: &str, wake_id: &str, created_ms: i64) {
        let mut state = self.lock();
        if state
            .announcements
            .get(session_id)
            .is_some_and(|mark| mark.created_ms >= created_ms)
        {
            return;
        }
        state.announcements.insert(
            session_id.to_string(),
            WakeMark {
                wake_id: wake_id.to_string(),
                created_ms,
            },
        );
        self.write(&state);
    }

    /// Persist both sections through the shared sidecar writer. The file is
    /// kept even when both sections are empty: its presence is the one-time
    /// migration marker that stops [`Self::load`] folding the legacy files
    /// again (ADR-0069).
    fn write(&self, state: &State) {
        #[cfg(test)]
        self.writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        sidecar::store(
            &self.path,
            "Chain Record",
            &ChainRecordFile {
                records: state.records.clone(),
                announcements: state.announcements.clone(),
            },
            false,
        );
    }

    /// Test-only: how often this store persisted its sidecar (spec #561's
    /// write-cost measurement). Reads the counter the test builds carry.
    #[cfg(test)]
    pub(crate) fn writes(&self) -> usize {
        self.writes.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Drop `session_id`'s durable record once its terminal card's ending write
/// is **confirmed** (ADR-0063 amendment; the pre-0067 rule dropped it before
/// the PATCH). A terminal card whose newest write failed recoverably is
/// still owed a Pending Card Update (ADR-0067), so its record stays: this
/// life's drain, or the next restart's reap, repairs the card and the reap
/// cleans the record once the write confirmed. A yielded `Waiting` card
/// keeps it (the reap settles its true end later) and a live card keeps it
/// (still owed). A card whose accumulator still holds an UNRENDERED orphan gap
/// keeps it too (spec #561, review #569): the durable fact on the record is the
/// only record of the tail that will land in it, so releasing it would strand
/// that tail.
/// Called after every ending PATCH in the flush path and by the
/// reap's same-card probe; the rule lives here, beside the record it releases.
pub(crate) async fn release_spent(cards: &CardsHandle, session_id: &str) {
    let card_message_id = {
        let cards = cards.cards.lock().await;
        let Some(card) = cards.get(session_id) else {
            return;
        };
        if !card.is_terminal() || card.owes_pending_gap() {
            return;
        }
        card.card_message_id().map(str::to_string)
    };
    if let Some(card_message_id) = card_message_id
        && cards.feishu.has_pending_card_update(&card_message_id)
    {
        return;
    }
    cards.chains.release(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "chain_records.json";

    /// The orphan gap is chain-level (spec #561, review #569): a re-point —
    /// a split continuation, a Wake continuation, a fresh Turn's takeover —
    /// carries the owed tail, a confirmed partial body advances it, and only a
    /// body that reached the gap's end consumes it.
    #[test]
    fn the_pending_gap_persists_advances_and_is_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track("ses_a", "om_old", MessageId::new("msg_cola_1"), Some(1_000), None);
        let gap = PendingGap {
            cursor: RenderedCursor::default(),
            anchor: TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1_000,
            },
            bound: Some(MessageId::new("msg_cola_2")),
        };
        assert!(chains.note_pending_gap("ses_a", "om_old", &gap));
        assert_eq!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .pending_gap,
            Some(gap.clone()),
            "the gap survives a restart"
        );

        chains.track_carrying_cursor(
            "ses_a",
            "om_new",
            MessageId::new("msg_cola_2"),
            Some(2_000),
            None,
            &RenderedCursor::default(),
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").pending_gap,
            Some(gap.clone()),
            "a projection's re-point carries the gap"
        );

        let frontier = CursorFrontier {
            message_id: MessageId::new("msg_a_1000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(1_500),
            delivered_chars: 5,
            prefix_digest: Some(cursor_prefix_digest("尾巴")),
        };
        assert!(chains.advance_pending_gap("ses_a", "om_new", &frontier, false));
        assert_eq!(
            chains
                .get("ses_a")
                .expect("the record")
                .pending_gap
                .expect("the gap stays owed")
                .cursor
                .frontier,
            Some(frontier.clone()),
            "a partial body advances the gap's cursor"
        );

        chains.track(
            "ses_a",
            "om_third",
            MessageId::new("msg_cola_3"),
            Some(3_000),
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").pending_gap,
            Some(PendingGap {
                cursor: RenderedCursor {
                    frontier: Some(frontier.clone()),
                    ..RenderedCursor::default()
                },
                ..gap.clone()
            }),
            "an ordinary re-point carries the advanced gap"
        );

        assert!(chains.advance_pending_gap("ses_a", "om_third", &frontier, true));
        assert!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .pending_gap
                .is_none(),
            "the body that reached the gap's end consumes it"
        );
        chains.track(
            "ses_a",
            "om_fourth",
            MessageId::new("msg_cola_4"),
            Some(4_000),
            None,
        );
        assert!(
            chains.get("ses_a").expect("the record").pending_gap.is_none(),
            "a consumed gap does not come back on a re-point"
        );
    }

    /// The projection's write-ahead intent (spec #561, review #569) persists
    /// across a restart, clears on demand, and is scoped to the record's card:
    /// a re-point (or a clear naming another card) never touches a newer
    /// generation.
    #[test]
    fn the_projection_intent_persists_clears_and_stays_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track("ses_a", "om_old", MessageId::new("msg_cola_1"), Some(1_000), None);

        assert!(chains.note_projection_intent("ses_a", "om_old"));
        assert!(
            chains.get("ses_a").expect("the record").projection_intent,
            "the intent is set in memory"
        );
        assert!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .projection_intent,
            "the intent survives a restart"
        );
        assert!(
            !chains.clear_projection_intent("ses_a", "om_other"),
            "a clear for another card never touches this record"
        );
        assert!(chains.get("ses_a").expect("the record").projection_intent);
        assert!(chains.clear_projection_intent("ses_a", "om_old"));
        assert!(
            !ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .projection_intent,
            "the clear persists"
        );

        // A re-point CARRIES the unresolved intent (spec #561, review #569):
        // the create it marked may have landed, and a takeover that erased the
        // mark would let a later restart post that tail again.
        assert!(chains.note_projection_intent("ses_a", "om_old"));
        chains.track("ses_a", "om_new", MessageId::new("msg_cola_1"), Some(1_000), None);
        assert!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the re-pointed record")
                .projection_intent,
            "a re-point carries the unresolved intent"
        );
        // The landed create's late-card collect consumes it on the winning
        // chain, whatever card that chain now names.
        assert!(chains.clear_projection_intent_any("ses_a"));
        assert!(
            !ChainRecords::load(path)
                .get("ses_a")
                .expect("the record")
                .projection_intent,
            "the consumed intent stays consumed"
        );
        assert!(
            !chains.clear_projection_intent_any("ses_a"),
            "consuming an absent intent writes nothing"
        );
    }

    #[test]
    fn track_persists_and_reloads_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        assert_eq!(
            chains.track(
                "ses_a",
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000),
                None
            ),
            None
        );
        chains.track("ses_b", "om_card_2", MessageId::new("msg_cola_2"), None, None);

        let reloaded = ChainRecords::load(path.clone());
        assert_eq!(
            reloaded.get("ses_a"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            reloaded.get("ses_b"),
            Some(ChainRecord::new("om_card_2", MessageId::new("msg_cola_2"), None))
        );
        assert_eq!(reloaded.get("ses_c"), None);

        // Re-tracking returns the previous record and keeps one per session.
        let previous = chains.track(
            "ses_a",
            "om_card_3",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        assert_eq!(
            previous,
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            ChainRecords::load(path.clone()).get("ses_a"),
            Some(ChainRecord::new(
                "om_card_3",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );

        // Releasing the last record keeps the file (the migration marker) but
        // drops the records themselves.
        chains.release("ses_a");
        chains.release("ses_b");
        assert!(path.exists(), "the Chain Record file is kept when empty");
        assert_eq!(chains.entries().len(), 0);
        assert_eq!(ChainRecords::load(path).entries().len(), 0);
    }

    /// The release entry is the one removal: the record is gone from memory and
    /// stays gone across a reload (a later load cannot resurrect it from the
    /// file).
    #[test]
    fn release_removes_the_record_and_persists_across_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );

        chains.release("ses_a");
        chains.release("ses_missing");
        assert_eq!(chains.get("ses_a"), None);
        assert_eq!(
            ChainRecords::load(path).get("ses_a"),
            None,
            "the removal persists across a reload"
        );
    }

    /// [`ChainRecords::release_if_card`] releases only the record that still
    /// names the card: a chain that moved on — a fresh Turn taking the session
    /// over, review #569 — keeps its record.
    #[test]
    fn release_if_card_drops_only_the_record_naming_that_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );

        assert!(
            !chains.release_if_card("ses_a", "om_stale"),
            "another card's record is never released"
        );
        assert_eq!(
            chains.get("ses_a").expect("the record survives").card_message_id,
            "om_card_1"
        );

        assert!(
            chains.release_if_card("ses_a", "om_card_1"),
            "the record naming the settled card is released"
        );
        assert_eq!(chains.get("ses_a"), None);
        assert_eq!(
            ChainRecords::load(path).get("ses_a"),
            None,
            "the guarded removal persists across a reload"
        );
    }

    #[test]
    fn rename_rekeys_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_old",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );

        chains.rename("ses_old", "ses_fresh");
        assert_eq!(chains.get("ses_old"), None);
        assert_eq!(
            ChainRecords::load(path).get("ses_fresh"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
    }

    /// A 404 recreate names the same logical Session: its announcement moves
    /// with the record, so the fresh id cannot re-announce a Wake the old id
    /// already showed.
    #[test]
    fn rename_rekeys_the_announcement_with_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_old",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        chains.advance("ses_old", "msg_wake_1", 1_000);

        chains.rename("ses_old", "ses_fresh");
        let reloaded = ChainRecords::load(path);
        assert_eq!(
            reloaded.announced("ses_fresh"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            })
        );
        assert_eq!(reloaded.announced("ses_old"), None);
    }

    #[test]
    fn a_corrupt_or_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert_eq!(ChainRecords::load(path.clone()).get("ses_a"), None);

        std::fs::write(&path, "not json").unwrap();
        let chains = ChainRecords::load(path.clone());
        assert_eq!(chains.get("ses_a"), None, "a corrupt file reads as empty");
        // The next write replaces the corrupt file with a valid one.
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        assert_eq!(
            ChainRecords::load(path).get("ses_a"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
    }

    /// The waiting mark is in-memory only: it survives the record's own life
    /// but never persists, and it only ever marks the card the record names.
    #[test]
    fn the_waiting_mark_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );

        assert!(!chains.mark_waiting_reaped("ses_a", "om_other"));
        assert!(!chains.get("ses_a").unwrap().waiting_reaped);
        assert!(chains.mark_waiting_reaped("ses_a", "om_card_1"));
        assert!(chains.get("ses_a").unwrap().waiting_reaped);

        // A reload (a restart) forgets it; a rewrite (a new live card) resets it.
        assert!(
            !ChainRecords::load(path.clone())
                .get("ses_a")
                .unwrap()
                .waiting_reaped
        );
        chains.track(
            "ses_a",
            "om_card_2",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        assert!(!chains.get("ses_a").unwrap().waiting_reaped);
    }

    /// #522: the given-up stamp mark is in-memory only and card-scoped — once
    /// set, no attempt may be claimed again for that card; a released claim
    /// (a transient failure) stays retryable, a reload (a restart) and a
    /// rewrite (a new live card) both forget the mark.
    #[test]
    fn the_rejected_stamp_mark_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );

        // A released claim (the transient-failure path) may be retried.
        assert!(chains.begin_restart_stamp("ses_a", "om_card_1"));
        chains.finish_restart_stamp("ses_a", "om_card_1");
        assert!(chains.begin_restart_stamp("ses_a", "om_card_1"));
        chains.finish_restart_stamp("ses_a", "om_card_1");

        assert!(!chains.mark_restart_stamp_rejected("ses_a", "om_other"));
        assert!(!chains.get("ses_a").unwrap().restart_stamp_rejected);
        assert!(chains.mark_restart_stamp_rejected("ses_a", "om_card_1"));
        assert!(chains.get("ses_a").unwrap().restart_stamp_rejected);
        assert!(
            !chains.begin_restart_stamp("ses_a", "om_card_1"),
            "a given-up stamp is never claimed again"
        );

        // A reload (a restart) forgets it; a rewrite (a new live card) resets it.
        assert!(
            !ChainRecords::load(path.clone())
                .get("ses_a")
                .unwrap()
                .restart_stamp_rejected
        );
        chains.track(
            "ses_a",
            "om_card_2",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        assert!(!chains.get("ses_a").unwrap().restart_stamp_rejected);
    }

    /// ADR-0068's takeover keep rule is in-memory only and scoped to the
    /// predecessor card it was recorded for: the #443 stamp's repair asks for
    /// the orphan's id and gets `None` for any other card, a restart forgets
    /// it, and a rewrite (a later handover) resets it.
    #[test]
    fn the_takeover_keep_rule_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_successor",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );

        assert_eq!(chains.predecessor_keep_strip("ses_a", "om_old"), None);
        chains.note_predecessor_keep("ses_a", "om_old", &["call_1".to_string()]);
        assert_eq!(
            chains.predecessor_keep_strip("ses_a", "om_old"),
            Some(vec!["call_1".to_string()])
        );
        assert_eq!(
            chains.predecessor_keep_strip("ses_a", "om_other"),
            None,
            "the rule only answers for the card it was recorded for"
        );
        chains.note_predecessor_keep("ses_a", "om_old", &[]);
        assert_eq!(
            chains.predecessor_keep_strip("ses_a", "om_old"),
            Some(Vec::new()),
            "a second takeover collect replaces the recorded rule"
        );

        // A reload (a restart) forgets it; a rewrite (a later handover) resets it.
        assert_eq!(
            ChainRecords::load(path.clone()).predecessor_keep_strip("ses_a", "om_old"),
            None
        );
        chains.track(
            "ses_a",
            "om_next",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        assert_eq!(chains.predecessor_keep_strip("ses_a", "om_old"), None);
    }

    /// The stored directory round-trips (the reap's V1 route); an empty string
    /// is unknown, exactly like `None`.
    #[test]
    fn the_directory_round_trips_and_an_empty_one_reads_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            Some("/work"),
        );
        chains.track("ses_b", "om_card_2", MessageId::new("msg_cola_2"), None, Some(""));

        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a").unwrap().directory.as_deref(), Some("/work"));
        assert_eq!(reloaded.get("ses_b").unwrap().directory, None);
    }

    #[test]
    fn the_anchor_travels_only_once_its_server_time_is_known() {
        let anchorless = ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), None);
        assert_eq!(anchorless.anchor(), None);
        let anchored = ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000));
        assert_eq!(
            anchored.anchor(),
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1_000,
            })
        );
    }

    #[test]
    fn advance_persists_and_reloads_the_newest_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        chains.advance("ses_a", "msg_wake_1", 1_000);
        // Older or equal sends do not move the mark.
        chains.advance("ses_a", "msg_wake_0", 900);
        chains.advance("ses_a", "msg_wake_1b", 1_000);
        chains.advance("ses_b", "msg_wake_2", 2_000);

        let reloaded = ChainRecords::load(path);
        assert_eq!(
            reloaded.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            })
        );
        assert_eq!(
            reloaded.announced("ses_b"),
            Some(WakeMark {
                wake_id: "msg_wake_2".into(),
                created_ms: 2_000,
            })
        );
        assert_eq!(reloaded.announced("ses_c"), None);
    }

    /// The two sections have independent lifetimes: removing a Session's
    /// record leaves its announcement in place.
    #[test]
    fn the_announcements_outlive_the_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        chains.advance("ses_a", "msg_wake_1", 1_000);

        chains.release("ses_a");
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a"), None);
        assert_eq!(
            reloaded.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            }),
            "the announcement is not removed with the record"
        );
    }

    /// ADR-0069's migration: without `chain_records.json`, the legacy sidecars
    /// are folded in once; afterwards they are never read again, so a record
    /// removed in the new store cannot be resurrected from the stale files.
    #[test]
    fn folds_the_legacy_sidecars_once_and_never_refolds_a_cleared_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(
            dir.path().join("live_cards.json"),
            r#"{"sessions":{"ses_a":{"card_message_id":"om_card_1","message_id":"msg_cola_1","created_ms":1000,"directory":"/work"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("wake_watermarks.json"),
            r#"{"sessions":{"ses_a":{"wake_id":"msg_wake_1","created_ms":1000}}}"#,
        )
        .unwrap();

        let chains = ChainRecords::load(path.clone());
        assert!(
            path.exists(),
            "the fold materializes the Chain Record file immediately"
        );
        assert_eq!(
            chains.get("ses_a"),
            Some(
                ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000))
                    .with_directory(Some("/work".into()))
            ),
            "the legacy record is folded in"
        );
        assert_eq!(
            chains.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            }),
            "the legacy watermark is folded in"
        );

        // Clearing the folded record persists the new file; the stale legacy
        // record must not come back on the next load.
        chains.release("ses_a");
        assert!(path.exists());
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a"), None);
    }

    /// The fold settles the migration at the first load: the marker file is
    /// materialized then, so the legacy files can vanish before any later
    /// mutation without losing the folded facts.
    #[test]
    fn the_fold_materializes_the_marker_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let legacy = dir.path().join("live_cards.json");
        std::fs::write(
            &legacy,
            r#"{"sessions":{"ses_a":{"card_message_id":"om_card_1","message_id":"msg_cola_1"}}}"#,
        )
        .unwrap();

        let chains = ChainRecords::load(path.clone());
        assert!(path.exists(), "the first load writes the merged file");
        assert_eq!(chains.get("ses_a").unwrap().card_message_id, "om_card_1");

        // The legacy file is no longer needed; a restart reads only the new
        // file.
        std::fs::remove_file(&legacy).unwrap();
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a").unwrap().card_message_id, "om_card_1");
    }

    /// The Rendered Cursor (spec #561) round-trips on its record, advances
    /// only for the card the record names, and leaves with the record.
    #[test]
    fn the_rendered_cursor_round_trips_and_advances_only_the_named_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        assert_eq!(chains.cursor("ses_a"), None, "a fresh record is cursorless");

        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 2,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: 42,
                prefix_digest: Some(7),
            }),
            live_calls: ["call_1".to_string()].into_iter().collect(),
        };
        chains.advance_cursor("ses_a", "om_other", &cursor);
        assert_eq!(chains.cursor("ses_a"), None, "a stale write advances nothing");

        chains.advance_cursor("ses_a", "om_card_1", &cursor);
        assert_eq!(
            ChainRecords::load(path.clone()).cursor("ses_a"),
            Some(cursor.clone()),
            "the cursor persists across a reload"
        );

        chains.release("ses_a");
        assert_eq!(
            ChainRecords::load(path).cursor("ses_a"),
            None,
            "the cursor is released with the record"
        );
    }

    /// A re-point within the chain (`track`) carries the cursor; a session
    /// with no previous record starts cursorless, and so does a record an
    /// older release wrote (no cursor field).
    #[test]
    fn track_carries_the_rendered_cursor_and_a_legacy_record_reads_cursorless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 0,
                kind: CursorPartKind::Reasoning,
                started_at: None,
                delivered_chars: 7,
                prefix_digest: Some(7),
            }),
            live_calls: ["call_1".to_string()].into_iter().collect(),
        };
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        chains.advance_cursor("ses_a", "om_card_1", &cursor);

        let previous = chains
            .track(
                "ses_a",
                "om_card_2",
                MessageId::new("msg_cola_1"),
                Some(1_000),
                None,
            )
            .expect("the re-point returns the previous record");
        assert_eq!(previous.cursor, Some(cursor.clone()));
        assert_eq!(
            chains.cursor("ses_a"),
            Some(cursor.clone()),
            "the split continuation's record inherits the chain's cursor"
        );

        chains.track("ses_b", "om_card_3", MessageId::new("msg_cola_2"), None, None);
        assert_eq!(
            chains.cursor("ses_b"),
            None,
            "a genuinely new chain starts cursorless"
        );

        // A record written without the cursor field (an older release) folds
        // in cursorless and can still be advanced by this life.
        std::fs::write(
            dir.path().join("legacy.json"),
            r#"{"records":{"ses_legacy":{"card_message_id":"om_card","message_id":"msg_cola_1"}},"announcements":{}}"#,
        )
        .unwrap();
        let legacy = ChainRecords::load(dir.path().join("legacy.json"));
        assert_eq!(legacy.cursor("ses_legacy"), None);
        assert_eq!(
            legacy.get("ses_legacy").unwrap().card_message_id,
            "om_card",
            "the legacy record still reads"
        );
    }

    /// Once `chain_records.json` exists, it wins: the legacy files beside it
    /// are never folded again.
    #[test]
    fn the_chain_file_wins_over_the_legacy_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track("ses_new", "om_new", MessageId::new("msg_cola_new"), None, None);
        std::fs::write(
            dir.path().join("live_cards.json"),
            r#"{"sessions":{"ses_old":{"card_message_id":"om_old","message_id":"msg_cola_old"}}}"#,
        )
        .unwrap();

        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_new").unwrap().card_message_id, "om_new");
        assert_eq!(reloaded.get("ses_old"), None);
    }

    /// The Chain Generation (spec #571, ticket #572): one in-memory monotonic
    /// version per chain, minted when the tracked card identity changes. A
    /// fresh chain starts at zero; a plain re-point onto a new card bumps by
    /// one.
    #[test]
    fn a_track_re_point_bumps_the_chain_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        chains.track(
            "ses_a",
            "om_first",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").generation,
            0,
            "a fresh chain starts at zero"
        );

        chains.track(
            "ses_a",
            "om_second",
            MessageId::new("msg_cola_2"),
            Some(2_000),
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the re-pointed record").generation,
            1,
            "a re-point onto a new card bumps the generation"
        );

        // A re-track of the SAME card is not an identity change: the record
        // keeps its generation (and a monotonic version can never drop back).
        chains.track(
            "ses_a",
            "om_second",
            MessageId::new("msg_cola_2"),
            Some(2_000),
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the re-tracked record").generation,
            1,
            "re-tracking the same card carries the generation"
        );
    }

    /// The two takeover entries mint the same way: a fresh Turn's takeover and
    /// the projection's atomic re-point bump the generation when they rewrite
    /// the record onto a different card.
    #[test]
    fn a_takeover_and_a_projection_re_point_bump_the_chain_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        chains.track("ses_a", "om_old", MessageId::new("msg_cola_1"), Some(1_000), None);
        chains.track_takeover("ses_a", "om_new", MessageId::new("msg_cola_2"), Some(2_000), None);
        assert_eq!(
            chains.get("ses_a").expect("the successor").generation,
            1,
            "a fresh Turn's takeover bumps the generation"
        );

        chains.track_carrying_cursor(
            "ses_a",
            "om_projected",
            MessageId::new("msg_cola_3"),
            Some(3_000),
            None,
            &RenderedCursor::default(),
            None,
        );
        assert_eq!(
            chains
                .get("ses_a")
                .expect("the projection's successor")
                .generation,
            2,
            "the projection's carrying re-point bumps again"
        );
    }

    /// The unrelated record writes — the anchor fill-in and the cursor
    /// advance — mutate the record in place, so the version the last identity
    /// change minted survives them untouched.
    #[test]
    fn anchor_and_cursor_writes_never_bump_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track("ses_a", "om_first", MessageId::new("msg_cola_1"), None, None);
        chains.track("ses_a", "om_second", MessageId::new("msg_cola_2"), None, None);
        let minted = chains.get("ses_a").expect("the record").generation;
        assert_eq!(minted, 1, "the re-point minted a version");

        chains.set_anchor(
            "ses_a",
            "om_second",
            &TurnAnchor {
                message_id: MessageId::new("msg_cola_2"),
                created_ms: 2_000,
            },
        );
        assert_eq!(
            chains.get("ses_a").expect("the anchored record").generation,
            minted,
            "the anchor fill-in never bumps the generation"
        );

        chains.advance_cursor("ses_a", "om_second", &RenderedCursor::default());
        assert_eq!(
            chains.get("ses_a").expect("the advanced record").generation,
            minted,
            "the cursor advance never bumps the generation"
        );
    }

    /// The record snapshot decisions read carries the version it was read at:
    /// every entry [`ChainRecords::entries`] hands out, and the predecessor a
    /// re-point returns, name their own generation — not the newest one.
    #[test]
    fn the_record_snapshot_carries_the_generation_it_was_read_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track("ses_a", "om_first", MessageId::new("msg_cola_1"), None, None);
        let previous = chains
            .track("ses_a", "om_second", MessageId::new("msg_cola_2"), None, None)
            .expect("the re-point returns the predecessor");
        assert_eq!(
            previous.generation, 0,
            "the predecessor carries the version its decision read"
        );

        let entries: HashMap<String, ChainRecord> = chains.entries().into_iter().collect();
        assert_eq!(
            entries["ses_a"].generation, 1,
            "the snapshot carries the record's current version"
        );
        assert_eq!(
            chains.generation("ses_a"),
            1,
            "the live read names the chain's current version"
        );
        assert_eq!(
            chains.generation("ses_missing"),
            0,
            "a chain with no record has no version to submit under"
        );
    }

    /// The generation is process-local: releasing the record drops it (a chain
    /// re-tracked after a release starts at zero again), and so does a fresh
    /// load — no durable field carries the version across lives.
    #[test]
    fn the_generation_dies_with_the_record_and_a_fresh_load_starts_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track("ses_a", "om_first", MessageId::new("msg_cola_1"), None, None);
        chains.track("ses_a", "om_second", MessageId::new("msg_cola_2"), None, None);
        assert_eq!(chains.get("ses_a").expect("the record").generation, 1);

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(
            !persisted.contains("\"generation\""),
            "the generation never reaches the durable file: {persisted}"
        );
        assert_eq!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .generation,
            0,
            "a fresh load starts at zero"
        );

        chains.release("ses_a");
        chains.track("ses_a", "om_third", MessageId::new("msg_cola_3"), None, None);
        assert_eq!(
            chains
                .get("ses_a")
                .expect("the chain after the release")
                .generation,
            0,
            "the released version never leaks into the next chain"
        );
    }
}
