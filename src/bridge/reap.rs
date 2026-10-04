//! Session Sync's durable Live Card reap (ADR-0063, #438): every tick, each
//! record in [`LiveCards`](crate::bridge::live_cards::LiveCards) is reconciled
//! against the Session's own reads, so a card a cola restart orphaned stops
//! looking live.
//!
//! The pass reaps *state*, never content: a readable transcript's real ending
//! settles the card in place (✅ / ❌ / ⏳ 等待后台任务), an idle Session whose
//! Turn message never landed ends it 「⚠️ 这条消息未被接收」 — never ✅ — and a
//! still-live Session keeps the record, its orphaned card stamped once with
//! the restart status (#443). A card a successor took over is
//! collected as 「⏳ 已由新卡片接管 · 已停止更新」 at the takeover itself
//! ([`collect_orphan`], called by the card paths that arm over an orphan), so
//! two cards never both look live. Every ending PATCH keeps the card's
//! already-rendered body best-effort (#434 acceptance feedback): the reap reads
//! the card's own view, strips the controls a whole-card read cannot preserve
//! and restamps the header over the kept elements; a failed read degrades to
//! the bare ending. The still-live orphan's one-time stamp (#443) reads the
//! same view the other way around: only the header changes, and a failed read
//! or PATCH claims nothing — a bare stamp would wipe the body the stamp exists
//! to keep — so the next pass retries it, under the pass's request bound (a
//! stuck Feishu request must never freeze Session Sync). Work that landed
//! while cola was down is published by the existing continuation machinery (a
//! Wake's continuation card) or simply left in the transcript: content the
//! card never showed is never rebuilt, and the reap never replays a turn onto
//! a stale card.
//!
//! A Session that relocated while the run was in flight (#428: `session_move`
//! into a git worktree) gains one line naming the move when its card reaches a
//! terminal ending (#439): the record's directory — the mapping's when the
//! record carries none — is the baseline, the store's session list is the
//! current fact where the generation exposes the canonical one, and a
//! difference between them is the move. That explains the interruption the
//! ending alone would leave mysterious, and it carries no chat content — only
//! the new directory, a server fact. A Waiting yield is not a settle and gets
//! no line; V1 move awareness is #433's, so on a V1 server without the
//! experimental list route the line may simply not render.
//!
//! Every action leaves one INFO line carrying the session and the decision —
//! never chat content; a settle that named a move says so too. A deciding read
//! the reap could not make, or could not interpret, claims nothing: the record
//! stays for the next tick. That covers an unrecognised status kind (unknown
//! is not idle) and a record with no directory to route the reads by (a
//! cwd-routed read could be another instance's run on V1). The move line's own
//! read is cosmetic: a failed or missed one only means the ending carries no
//! line, never that the ending is withheld. Neither PATCH takes a session's
//! card-write lock: no in-memory accumulator owns the card it targets (that is
//! the reap's precondition) and the ending is computed whole, then PATCHed
//! once, so there is no read-send-record sequence to serialize.

use crate::backend::TurnSettle;
use crate::bridge::handles::{CardsHandle, FlowHandles};
use crate::bridge::live_cards::LiveCard;
use crate::bridge::turn::Turn;
use crate::feishu::card::{CardState, error_line, move_line, shell::CardBuilder};

/// Collect the orphaned card `card_message_id` because a new card took the
/// chain over (ADR-0063): one PATCH naming the successor, terminal and grey,
/// keeping whatever the card already showed (#434 acceptance feedback). A
/// failed PATCH only warns; the record follows the successor either way, so the
/// freeze it leaves behind is the pre-#438 behavior, never a crash.
pub(crate) async fn collect_orphan(cards: &CardsHandle, session_id: &str, card_message_id: &str) {
    if card_message_id.is_empty() {
        return;
    }
    let card = ending_card(CardState::TakenOver, None, None);
    match patch_ending_keeping_body(cards.feishu.as_ref(), card_message_id, &card).await {
        // The one reap vocabulary: the INFO line's ending word comes from the
        // state itself, exactly like every `ReapPass::settle` line.
        Ok(()) => tracing::info!(
            "live-card reap: session {session_id} {}",
            CardState::TakenOver.reap_word()
        ),
        Err(e) => tracing::warn!(
            "live-card reap: session {session_id} could not collect card {card_message_id}: {e}"
        ),
    }
}

/// Reconcile one record against the Session's own reads (ADR-0063). `directory`
/// is the Session's mapped directory, when it still has one — the fallback
/// route when the record itself carries none. `tracked_directory` is the
/// directory the card was tracked under when the caller knows it independently
/// of the current mapping (the pre-follow directory, #433), used for the move
/// verdict (#439) when the record carries no directory of its own. `record` is
/// the snapshot the caller took from the sidecar. `read_timeout_ms` bounds each
/// of the reads, the Session Sync pass's own request bound (injectable in
/// tests), so a hung server degrades to "nothing claimed" instead of freezing
/// the tick.
pub(crate) async fn reconcile(
    handles: &FlowHandles,
    session_id: &str,
    directory: Option<&str>,
    tracked_directory: Option<&str>,
    record: &LiveCard,
    read_timeout_ms: u64,
) {
    // A live Turn (or the follow that inherited its guard) owns the session,
    // and an inbound message is about to: either way the card is not orphaned.
    if handles.waits.inflight.lock().await.contains(session_id)
        || handles.waits.inbound_pending(session_id).await
    {
        return;
    }
    // Why not [`Turn::chain_ownership`]? Routing and the Wake step need only
    // "owned or not"; a reconcile needs the record's card id and the lifecycle
    // distinctions below — keep a live or yielded record, drop a spent one,
    // collect a lagged one and re-point. The two agree where it matters: the
    // orphan branch below runs only when this process holds no card identity
    // for the session (and the record can never name a card this process is
    // mid-admitting: `take_over_card` attaches the id before tracking), while
    // a card this process still holds is never PATCHed by the ladder — a
    // Waiting card included, which routing reads as unowned but whose true end
    // this process's ledger watch still owes. A future ownership rule must be
    // mirrored here, or the reap restructured to consume the verdict, rather
    // than assumed to reach it.
    // What does THIS process know about the session's card?
    let current_id = Turn::card_message_id(&handles.cards, session_id).await;
    // A live or yielded card is still running; a terminal one (and a missing
    // card) is not.
    let current_running = Turn::is_running(&handles.cards, session_id).await;
    if let Some(current_id) = current_id {
        if current_id == record.card_message_id {
            // The recorded card IS this process's card: its own lifecycle owns
            // it. A terminal card's record is spent once its ending write is
            // confirmed (ADR-0063 amendment — a write still pending in the
            // outbox keeps the record); a live or yielded card keeps it.
            if !current_running {
                Turn::discard_spent_record(&handles.cards, session_id).await;
            }
            return;
        }
        // A successor card owns the session while the record still names
        // another card — the record write raced the handover, or a path
        // attached the new id without tracking. Collect the recorded card in
        // place (ADR-0063's goal: never leave a card looking live), then name
        // the live successor; a settled successor keeps no record. The collect
        // is best-effort and harmless when the handover already collected it.
        collect_orphan(&handles.cards, session_id, &record.card_message_id).await;
        match (
            current_running,
            Turn::armed_turn_anchor(&handles.cards, session_id).await,
        ) {
            (true, Some(anchor)) => {
                handles.cards.live_cards.replace(
                    session_id,
                    LiveCard::new(current_id, anchor.message_id.clone(), Some(anchor.created_ms))
                        // The route belongs to the session, not the card: keep
                        // it across the re-point.
                        .with_directory(record.directory.clone()),
                );
            }
            // A settled successor has nothing to track, and an anchorless one
            // cannot scope a reap: the next track writes it when it can.
            _ => handles.cards.live_cards.remove(session_id),
        }
        return;
    }
    // No card in this process owns the session: the restart orphan (or a chain
    // that vanished). The Session's own reads decide, and a read the reap
    // could not make claims nothing.
    //
    // The reads route by the directory the record carried from track time; the
    // mapping's is the fallback. With neither, the reap decides nothing: on a
    // generation whose reads are per-directory instance (V1) a cwd-routed
    // status could belong to a different instance's run, and stamping over a
    // live run is worse than leaving a record for a later life (growth is
    // bounded by the live-card sessions).
    // The directory the card was TRACKED under: the record's own when it has
    // one, else the caller's pre-follow value. The followed route can never
    // prove a move — it already names the new location (#433).
    let tracked = record
        .directory
        .as_deref()
        .filter(|directory| !directory.is_empty());
    let directory = tracked.or(directory);
    let Some(directory) = directory else {
        tracing::debug!(
            "live-card reap: session {session_id} has no directory to route its reads; keeping the record"
        );
        return;
    };
    let baseline_directory = tracked.or(tracked_directory).unwrap_or(directory);
    // One record's reconcile: the facts every ending — and the restart stamp
    // — reads, so the calls below carry only what differs (the ending and its
    // detail).
    let pass = ReapPass {
        handles,
        session_id,
        record,
        baseline_directory,
        read_timeout_ms,
    };
    let status = match crate::bridge::bounded_call(
        "live-card reap status",
        read_timeout_ms,
        handles.backend.session_status(session_id, Some(directory)),
    )
    .await
    {
        Some(Ok(status)) => status,
        Some(Err(e)) => {
            tracing::warn!("live-card reap: session {session_id} status read failed: {e}");
            return;
        }
        None => return,
    };
    // Only a definite non-live status decides an ending. An unrecognised
    // status kind (`Ok(None)`) is unknown, not idle: `never guessed` — the
    // record stays for the next tick.
    let Some(status) = status else {
        return;
    };
    // A live Session keeps the card: the run may still answer it. This
    // process holds no card for the session, so the record is a restart
    // orphan — its card froze when the previous process died and nothing
    // will move it until transcript truth ends it. Stamp that once per
    // process life (#443) so the user knows why it stopped moving: a failed
    // attempt claims nothing and is retried next pass.
    if status.is_live() {
        if !record.restarted_reaped {
            // Bounded like every other Session Sync request (the Feishu client
            // carries no default timeout): a stuck call must not freeze the
            // pass behind one orphan. A cut-loose attempt claims nothing and
            // is retried next pass; re-stamping is idempotent. The bound makes
            // ordering against a concurrent takeover best-effort — a
            // cancelled write may still land (ADR-0063's amendment; the same
            // at-least-once caveat as ADR-0067's drain) — while every
            // takeover this process can observe is covered by the stamp's
            // pre- and post-PATCH ownership checks.
            let _ = crate::bridge::bounded_call("live-card reap stamp", read_timeout_ms, async {
                pass.stamp_restarted().await;
                Ok(())
            })
            .await;
        }
        return;
    }
    let transcript = match crate::bridge::bounded_call(
        "live-card reap transcript",
        read_timeout_ms,
        handles.backend.transcript(session_id),
    )
    .await
    {
        Some(Ok(transcript)) => transcript,
        Some(Err(e)) => {
            tracing::warn!("live-card reap: session {session_id} transcript read failed: {e}");
            return;
        }
        None => return,
    };
    // The settle decision's scope: the recorded anchor, else the submitted
    // message's own anchor re-derived from this read (it may have landed after
    // the record's last write). Absent, the message never landed — the
    // anchorless Unreceived scope.
    let scope = record
        .anchor()
        .or_else(|| transcript.anchor_of_user(record.message_id.as_str()));
    match transcript.settle(scope.as_ref()) {
        // The read's boundary rule is unsatisfied (a Wake's Execution has not
        // closed): the ending is not decided — keep observing.
        TurnSettle::Running => {}
        TurnSettle::Complete => {
            pass.settle(CardState::Done, None).await;
        }
        TurnSettle::Failed(error) => {
            pass.settle(CardState::Error, Some(&error_line(&error))).await;
        }
        // The wait is still on: the card yields 「⏳ 等待后台任务」 and keeps its
        // record, so a later read (this pass, every tick) settles the true end.
        // The ending is PATCHed once per life; a restart re-stamps it once. A
        // yield is not a settle: it carries no move line.
        TurnSettle::Waiting => {
            if record.waiting_reaped {
                return;
            }
            if pass.settle(CardState::Waiting, None).await {
                handles
                    .cards
                    .live_cards
                    .mark_waiting_reaped(session_id, &record.card_message_id);
            }
        }
        // The submitted message never reached the transcript and the Session
        // is idle: nobody will answer it — never ✅ (ADR-0062).
        TurnSettle::Unreceived => {
            pass.settle(CardState::Unreceived, None).await;
        }
    }
}

/// One record's reconcile, as the facts every ending — and the restart stamp
/// (#443) — reads: the handles, the Session id, the record, the baseline
/// directory the move verdict compares against, and the pass's read bound.
/// Grouped so the settle and stamp calls carry only what differs between
/// them.
struct ReapPass<'a> {
    handles: &'a FlowHandles,
    session_id: &'a str,
    record: &'a LiveCard,
    baseline_directory: &'a str,
    read_timeout_ms: u64,
}

impl ReapPass<'_> {
    /// PATCH the record's card into `state` — keeping the card's existing body
    /// best-effort (#434 acceptance feedback) — and, when the state is terminal,
    /// drop the record: nothing is owed a reap any more. A terminal ending whose
    /// Session's current directory differs from the pass's baseline directory
    /// gains one extra line naming the move (#439), on top of `detail` (the
    /// failure's message when there is one); a Waiting yield is not a settle and
    /// carries none. Returns whether the PATCH landed (a failed one keeps the
    /// record for the next tick). One INFO line per action, naming the session
    /// and the decision — never chat content; a settle that named a move says
    /// so.
    async fn settle(&self, state: CardState, detail: Option<&str>) -> bool {
        let terminal = state.is_terminal();
        let move_note = if terminal { self.move_note().await } else { None };
        let card = ending_card(state.clone(), detail, move_note.as_deref());
        if let Err(e) = patch_ending_keeping_body(
            self.handles.cards.feishu.as_ref(),
            &self.record.card_message_id,
            &card,
        )
        .await
        {
            tracing::warn!(
                "live-card reap: session {} could not settle card {}: {e}",
                self.session_id,
                self.record.card_message_id
            );
            return false;
        }
        let moved = if move_note.is_some() {
            " on a session that moved"
        } else {
            ""
        };
        tracing::info!(
            "live-card reap: session {} {}{moved}",
            self.session_id,
            state.reap_word()
        );
        if terminal {
            self.handles.cards.live_cards.remove(self.session_id);
        }
        true
    }

    /// Stamp a still-live restart orphan (#443) — this record's card, owned by
    /// no renderer in this process, whose Session reads live: the run may
    /// still answer the card, but nothing will move it until transcript truth
    /// ends it, and the user deserves to know why. The card's own view is read
    /// back and only the header changes — body kept, controls stripped,
    /// exactly like a preserved ending — so the card keeps its body under the
    /// restart status. Unlike [`patch_ending_keeping_body`], a failed read or
    /// PATCH claims nothing: the stamp's whole value is the body it preserves,
    /// so a bare fallback would destroy the very thing it protects; the next
    /// pass retries. The whole stamp is bounded by the pass's request timeout
    /// at its call site — the Feishu client carries no default timeout — so a
    /// stuck request cannot freeze Session Sync behind one card, and a
    /// cut-loose attempt claims nothing either (a cancelled PATCH may have
    /// landed; re-stamping is idempotent). The in-memory mark is set only when
    /// the PATCH landed, so the stamp is one per process life; the later
    /// transcript-truth settle (or a successor's collect) reads the view again
    /// and replaces the header, superseding the stamp as any other card state.
    async fn stamp_restarted(&self) {
        let card_message_id = &self.record.card_message_id;
        let platform = self.handles.cards.feishu.as_ref();
        let view = match platform.get_card_view(card_message_id).await {
            Ok(view) => view,
            Err(e) => {
                tracing::warn!(
                    "live-card reap: session {} could not read card {card_message_id} to stamp the restart: {e}",
                    self.session_id
                );
                return;
            }
        };
        // The pass's ownership check is stale by now: a Turn may have started
        // — or a successor armed — while the view read was in flight. A
        // takeover attaches the successor id BEFORE it collects this card
        // (`take_over_card`'s attach-then-collect order), so any admitted
        // successor is visible here, and its collect is the later terminal:
        // the stamp yields to it rather than overwriting it with an interim
        // status.
        if Turn::card_message_id(&self.handles.cards, self.session_id)
            .await
            .is_some()
        {
            return;
        }
        let card = restamped_keeping_body(&ending_card(CardState::Restarted, None, None), &view);
        if let Err(e) = platform.update_message(card_message_id, &card).await {
            tracing::warn!(
                "live-card reap: session {} could not stamp card {card_message_id}: {e}",
                self.session_id
            );
            return;
        }
        tracing::info!(
            "live-card reap: session {} {}",
            self.session_id,
            CardState::Restarted.reap_word()
        );
        self.handles
            .cards
            .live_cards
            .mark_restarted_reaped(self.session_id, card_message_id);
        // A takeover that started while the PATCH was in flight may have seen
        // its older collect overwritten by this stamp (the delivery lock
        // serializes the two writes, not their order of intent): if a
        // successor owns the session now, collect the orphan again so the
        // takeover has the card's last word. A collect that lands after this
        // PATCH wins on its own; this only repairs the reversed order.
        if Turn::card_message_id(&self.handles.cards, self.session_id)
            .await
            .is_some()
        {
            collect_orphan(&self.handles.cards, self.session_id, card_message_id).await;
        }
    }

    /// The one line a settling card carries when the Session's location changed
    /// since the card was tracked (#428, #439): the move named, so an
    /// interruption the ending alone would leave mysterious is explained. The
    /// current directory comes from the Session reads, never from chat: the
    /// moved Session now lives under its new directory, so the directory-routed
    /// reads that got the reap here may not know it. The list read goes through
    /// the shared session-list cache (`SessionsHandle::cached_session_list`), so
    /// at most one settle per cache TTL touches the wire — but that miss is
    /// awaited before the PATCH, so on a hung server the ending can wait up to
    /// `read_timeout_ms`. That price buys a cosmetic line and is bounded; the
    /// ending itself is never withheld for it. A read that fails, a Session the
    /// list does not carry, an empty directory, or the baseline itself all
    /// claim nothing — no line, exactly the pre-#439 card. A project-scoped
    /// session list may omit the moved Session, and then no line renders
    /// either.
    async fn move_note(&self) -> Option<String> {
        let sessions = match crate::bridge::bounded_call(
            "live-card reap session list",
            self.read_timeout_ms,
            self.handles.sessions.cached_session_list(&self.handles.backend),
        )
        .await
        {
            Some(Ok(sessions)) => sessions,
            Some(Err(e)) => {
                tracing::warn!(
                    "live-card reap: session {} session list read failed: {e}",
                    self.session_id
                );
                return None;
            }
            None => return None,
        };
        let current = sessions
            .iter()
            .find(|session| session.id == self.session_id)?
            .directory
            .as_str();
        if current.is_empty() || current == self.baseline_directory {
            return None;
        }
        Some(move_line(current))
    }
}

/// A bare card carrying one ending — the reap's whole card vocabulary when the
/// card's own view cannot be read. The lost turn's content is deliberately NOT
/// rebuilt (ADR-0063): the ending's header, the failure's own message when
/// there is one, the move line when the Session relocated (#439), and no
/// action. On the PATCH path this bare ending is merged with the card's
/// currently rendered view best-effort ([`restamped_keeping_body`], #434
/// acceptance feedback): the card keeps whatever it already showed, while
/// content it never showed — the lost turn's missing work — is still never
/// rebuilt. The Unreceived ending is therefore actionless here — the restart
/// took the accumulator its 重新发起 click would claim (the original prompt and
/// the card session), so the button could only be dead; the user re-sends
/// instead (ADR-0062's amendment).
fn ending_card(state: CardState, detail: Option<&str>, move_note: Option<&str>) -> serde_json::Value {
    let mut builder = CardBuilder::new().with_state(state);
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        builder = builder.with_text(detail);
    }
    if let Some(note) = move_note {
        builder = builder.with_text(note);
    }
    builder.build()
}

/// PATCH `bare` onto `card_message_id`, keeping the card's existing body
/// best-effort (#434 acceptance feedback): read the card's own view, merge the
/// ending over it, PATCH the merge. A failed read PATCHes `bare` directly —
/// today's behavior — because the ending must never depend on the read.
///
/// A PATCH the platform *definitely* refuses as card content (the typed
/// `CardContentRejected`, e.g. `230099`) is retried once bare: the refusal is
/// deterministic, so the bare ending lands and the card never stays looking
/// live. Any other failure — transport, timeout, auth, server — is returned
/// as-is: the preserved PATCH may already have landed, and retrying bare would
/// then wipe the very body this path exists to keep.
async fn patch_ending_keeping_body(
    platform: &dyn crate::feishu::Platform,
    card_message_id: &str,
    bare: &serde_json::Value,
) -> crate::error::Result<()> {
    match platform.get_card_view(card_message_id).await {
        Ok(view) => {
            let card = restamped_keeping_body(bare, &view);
            match platform.update_message(card_message_id, &card).await {
                Ok(()) => Ok(()),
                Err(e @ crate::error::BridgeError::CardContentRejected { .. }) => {
                    tracing::warn!(
                        "live-card reap: card {card_message_id} refused the preserved ending ({e}); retrying bare"
                    );
                    platform.update_message(card_message_id, bare).await
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => {
            tracing::debug!("live-card reap: card {card_message_id} view unreadable ({e}); settling bare");
            platform.update_message(card_message_id, bare).await
        }
    }
}

/// The bare card — an ending, or the still-live orphan's restart stamp (#443)
/// — restamped over the card's existing view (#434 acceptance feedback): the
/// bare card's header leads, its own elements (the failure's message and/or
/// the move line; none for the stamp) come before the view's body, and every
/// interactive element is stripped from the view's elements — a whole-card read
/// does not return a control's `value`, so a preserved control could only
/// render dead. The view's `config` rules the restamped card (`streaming_mode`
/// forced off so a preserved card never keeps a live-streaming presentation);
/// a view without one keeps the bare card's. Schema 2.0, the one the PATCH API
/// accepts back.
fn restamped_keeping_body(bare: &serde_json::Value, view: &serde_json::Value) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> =
        bare["body"]["elements"].as_array().cloned().unwrap_or_default();
    if let Some(view_elements) = view["body"]["elements"].as_array() {
        elements.extend(view_elements.iter().filter_map(stripped_of_controls));
    }
    let mut config = view
        .get("config")
        .filter(|config| config.is_object())
        .cloned()
        .or_else(|| bare.get("config").filter(|config| config.is_object()).cloned())
        .unwrap_or_else(|| serde_json::json!({ "wide_screen_mode": true }));
    config["streaming_mode"] = serde_json::json!(false);
    serde_json::json!({
        "schema": "2.0",
        "config": config,
        "header": bare["header"].clone(),
        "body": { "elements": elements },
    })
}

/// The card components that are interactive, or exist only to group controls.
/// A preserved card must never show a control whose action can no longer run
/// (its callback `value` did not survive the whole-card read), so these are
/// stripped wherever they nest: panels, column sets and forms included. The
/// list is Feishu's interactive card-JSON-2.0 components (`checker` is the
/// documented 勾选器) plus the `form` container — `form` goes whole because an
/// emptied form is invalid and every child it can hold is stripped anyway.
const INTERACTIVE_TAGS: &[&str] = &[
    "button",
    "action",
    "input",
    "select_static",
    "multi_select_static",
    "select_person",
    "multi_select_person",
    "overflow",
    "date_picker",
    "picker_time",
    "picker_datetime",
    "select_img",
    "checker",
    "form",
];

/// Whether `tag` names an [`INTERACTIVE_TAGS`] component.
fn is_interactive_element(tag: &str) -> bool {
    INTERACTIVE_TAGS.contains(&tag)
}

/// [`is_interactive_element`]'s recursive application: `None` for a stripped
/// element, the element — its nested arrays and object fields filtered —
/// otherwise. An interactive child is dropped **whole**: leaving a `null`
/// placeholder would itself be invalid card JSON and get the preserved PATCH
/// rejected.
fn stripped_of_controls(element: &serde_json::Value) -> Option<serde_json::Value> {
    if element
        .get("tag")
        .and_then(|tag| tag.as_str())
        .is_some_and(is_interactive_element)
    {
        return None;
    }
    match element {
        serde_json::Value::Object(map) => {
            let mut cleaned = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if let Some(cleaned_value) = stripped_of_controls(value) {
                    cleaned.insert(key.clone(), cleaned_value);
                }
            }
            Some(serde_json::Value::Object(cleaned))
        }
        serde_json::Value::Array(items) => Some(serde_json::Value::Array(
            items.iter().filter_map(stripped_of_controls).collect(),
        )),
        other => Some(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reap's card vocabulary is one state + one optional line (plus the
    /// move line when the Session relocated): the ending's header, the failure
    /// message when given, and no recovery button (the restart took the
    /// accumulator that could claim one).
    #[test]
    fn ending_cards_carry_the_state_and_no_action() {
        let done = ending_card(CardState::Done, None, None);
        assert_eq!(done["header"]["title"]["content"], "✅ 完成");
        assert!(done["body"]["elements"].as_array().unwrap().is_empty());

        let failed = ending_card(CardState::Error, Some("**错误**: 503"), None);
        assert_eq!(failed["header"]["title"]["content"], "❌ 出错");
        assert!(
            failed.to_string().contains("503"),
            "the failure's own message rides the card: {failed}"
        );

        let unreceived = ending_card(CardState::Unreceived, None, None);
        assert_eq!(unreceived["header"]["title"]["content"], "⚠️ 这条消息未被接收");
        assert!(
            unreceived["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .all(|element| element["tag"] != "button"),
            "the reap offers no recovery action: {unreceived}"
        );

        let waiting = ending_card(CardState::Waiting, None, None);
        assert_eq!(waiting["header"]["title"]["content"], "⏳ 等待后台任务");
    }

    /// The move line (#439) rides the ending next to its other line: the header
    /// keeps the state, the failure's message and the new directory both
    /// render, and the line is built from the directory alone — no chat
    /// content.
    #[test]
    fn ending_cards_carry_the_move_line_next_to_the_detail() {
        let failed_and_moved = ending_card(
            CardState::Error,
            Some("**错误**: Step interrupted"),
            Some(&move_line("/work/.worktrees/zh-user-guide")),
        );
        assert_eq!(failed_and_moved["header"]["title"]["content"], "❌ 出错");
        let rendered = failed_and_moved.to_string();
        assert!(
            rendered.contains("Step interrupted"),
            "the failure's own message stays: {failed_and_moved}"
        );
        assert!(
            rendered.contains("**会话已迁移**: `/work/.worktrees/zh-user-guide`"),
            "the move line names the new directory: {failed_and_moved}"
        );
        assert_eq!(
            failed_and_moved["body"]["elements"].as_array().unwrap().len(),
            2,
            "the card carries the detail and the move line and nothing else: {failed_and_moved}"
        );

        let unreceived_moved = ending_card(CardState::Unreceived, None, Some(&move_line("/w2")));
        assert_eq!(
            unreceived_moved["body"]["elements"][0]["content"],
            "**会话已迁移**: `/w2`"
        );
    }

    /// The preserved ending (#434 acceptance feedback): the ending's header
    /// replaces the old one, the ending's own line is prepended to the fetched
    /// body, every interactive element is stripped recursively (a nested
    /// button and a whole action block included), and `streaming_mode` is
    /// forced off while the view's other config survives. A view without a
    /// config keeps the ending's.
    #[test]
    fn restamped_keeping_body_prepends_the_line_and_strips_controls() {
        use crate::bridge::test_support::{card_buttons, card_has_tag, card_text};

        let bare = ending_card(CardState::Error, Some("**错误**: 503"), None);
        let view = serde_json::json!({
            "schema": "2.0",
            "config": {
                "wide_screen_mode": true,
                "streaming_mode": true,
                "enable_forward_interaction": false
            },
            "header": {
                "template": "blue",
                "title": { "tag": "plain_text", "content": "✍️ 回复中" }
            },
            "body": { "elements": [
                { "tag": "markdown", "content": "**正文** 第一段" },
                { "tag": "collapsible_panel", "expanded": false, "elements": [
                    { "tag": "markdown", "content": "面板里的输出" },
                    { "tag": "button", "text": { "tag": "plain_text", "content": "重试" },
                      "value": { "action": "retry" } }
                ] },
                { "tag": "action", "actions": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "重新发起" },
                      "value": { "action": "resume" } }
                ] },
                { "tag": "hr" }
            ] }
        });

        let card = restamped_keeping_body(&bare, &view);
        assert_eq!(card["schema"], "2.0");
        assert_eq!(
            card["header"]["title"]["content"], "❌ 出错",
            "the ending's header replaces the card's old one: {card}"
        );
        assert_eq!(
            card["config"]["streaming_mode"], false,
            "streaming is forced off: {card}"
        );
        assert_eq!(
            card["config"]["wide_screen_mode"], true,
            "other config fields survive: {card}"
        );
        assert_eq!(card["config"]["enable_forward_interaction"], false);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["content"], "**错误**: 503",
            "the ending's own line is prepended: {card}"
        );
        assert_eq!(
            elements[1]["content"], "**正文** 第一段",
            "the preserved body follows the line: {card}"
        );
        assert!(
            card_text(&card).contains("面板里的输出"),
            "nested panel content survives: {card}"
        );
        assert!(
            card_buttons(&card).is_empty(),
            "no preserved button survives, nested or not: {card}"
        );
        assert!(!card_has_tag(&card, "action"), "no action block survives: {card}");
        assert_eq!(
            elements[2]["tag"], "collapsible_panel",
            "the panel itself survives: {card}"
        );
        assert_eq!(
            elements[2]["elements"].as_array().unwrap().len(),
            1,
            "the panel's nested control is stripped: {card}"
        );
        assert_eq!(
            elements[3]["tag"], "hr",
            "non-interactive elements keep their order: {card}"
        );

        // A view without a config keeps the ending's own (streaming still off).
        let no_config = serde_json::json!({
            "body": { "elements": [{ "tag": "markdown", "content": "旧的正文" }] }
        });
        let card = restamped_keeping_body(&bare, &no_config);
        assert_eq!(card["config"]["wide_screen_mode"], true);
        assert_eq!(card["config"]["streaming_mode"], false);
        assert_eq!(card["body"]["elements"][1]["content"], "旧的正文");
    }

    /// Nested and container controls: an interactive object at a non-array key
    /// is dropped **whole** (a `null` placeholder would itself be invalid card
    /// JSON), a whole `form` goes (an emptied form is invalid and every child
    /// it can hold is stripped anyway), and a `checker`/`select_img` nested in
    /// a column go too — while the display content around them survives.
    #[test]
    fn restamped_keeping_body_drops_containers_without_nulls() {
        use crate::bridge::test_support::{card_has_tag, card_text};

        fn contains_null(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::Null => true,
                serde_json::Value::Object(map) => map.values().any(contains_null),
                serde_json::Value::Array(items) => items.iter().any(contains_null),
                _ => false,
            }
        }

        let bare = ending_card(CardState::Done, None, None);
        let view = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "body": { "elements": [
                { "tag": "form", "name": "switch_search", "elements": [
                    { "tag": "input", "name": "search" },
                    { "tag": "button", "text": { "tag": "plain_text", "content": "搜索" },
                      "value": { "action": "submit" } }
                ] },
                { "tag": "collapsible_panel", "elements": [
                    { "tag": "markdown", "content": "面板里的输出" }
                ],
                  // An object-valued key holding a control: the key must go
                  // whole, not become `"button_area": null`.
                  "button_area": { "tag": "button", "value": { "action": "retry" } } },
                { "tag": "column_set", "columns": [
                    { "tag": "column", "elements": [
                        { "tag": "checker", "name": "check_1" },
                        { "tag": "select_img", "name": "pick_1" },
                        { "tag": "markdown", "content": "保留的正文" }
                    ] }
                ] }
            ] }
        });

        let card = restamped_keeping_body(&bare, &view);
        for tag in ["form", "input", "button", "checker", "select_img"] {
            assert!(!card_has_tag(&card, tag), "{tag} must be stripped whole: {card}");
        }
        assert!(
            !contains_null(&card),
            "no null placeholder may survive a stripped child: {card}"
        );
        assert!(
            card_text(&card).contains("面板里的输出") && card_text(&card).contains("保留的正文"),
            "the display content around the controls survives: {card}"
        );
        let panel = &card["body"]["elements"][0];
        assert_eq!(panel["tag"], "collapsible_panel", "{card}");
        assert!(
            panel.get("button_area").is_none(),
            "the object-valued control's key is dropped, never nulled: {card}"
        );
    }

    /// A bare ending with no line of its own prepends nothing: the preserved
    /// body leads the card.
    #[test]
    fn restamped_keeping_body_without_a_line_leads_with_the_view() {
        let bare = ending_card(CardState::Done, None, None);
        let view = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "body": { "elements": [{ "tag": "markdown", "content": "保留的正文" }] }
        });
        let card = restamped_keeping_body(&bare, &view);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0]["content"], "保留的正文");
        assert_eq!(card["header"]["title"]["content"], "✅ 完成");
    }
}
