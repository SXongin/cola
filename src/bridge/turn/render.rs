//! The Turn's render internals (spec #298, A2b).
//!
//! The render poll loop, the part rendering, and the session subtitle/title
//! refresh live here, behind the Turn's interface (`Turn::session_subtitle` /
//! `Turn::render_and_flush` in the parent module) and its private
//! [`RenderPoll`] task. Nothing here is reachable from outside the Turn module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::Instrument;

use crate::backend::{
    Part, SessionTranscript, TaskRetirementEnding, ToolStatus, TurnAnchor, Wake, WakeSource,
};
use crate::bridge::core::SESSION_INFO_TIMEOUT;
use crate::bridge::handles::{CardsHandle, RequestsHandle, SessionsHandle, TurnHandles};
use crate::bridge::span;
use crate::bridge::turn::state::{LedgerCadence, RenderedPart, StreamAccumulator};
use crate::config::ThreadKey;
use crate::feishu::card::ledger::{TaskCompletionEntry, TaskEnding, TaskKind};
use crate::feishu::card::tool_render::{ChildActivity, TaskLiveness};

use super::Turn;

/// The session/thread name shown as the card subtitle, formatted as
/// `<title> · <id-tail>` (e.g. "你好 · 01ba0ed"). The OpenCode server's OWN
/// session title (what OpenChamber shows) is the single source of truth
/// (ADR-0007) — fetched on demand, never cached locally. While the server
/// still has the default `New session - ...` title (or the title is empty),
/// the id-tail alone identifies the session; the current prompt is never
/// echoed (the reply context already shows it).
pub(super) async fn session_subtitle(
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    thread_key: &crate::config::ThreadKey,
    text: &str,
) -> String {
    let prompt_preview: String = text.chars().take(50).collect();
    let Some(entry) = sessions.active_entry(thread_key).await else {
        return String::new();
    };
    let session_id = entry.session_id.clone();
    let mut name = String::new();
    if let Ok(Ok(info)) = tokio::time::timeout(
        SESSION_INFO_TIMEOUT,
        backend
            .clone()
            .for_directory(&entry.directory)
            .session_info(&session_id),
    )
    .await
        && let Some(t) = info.title.filter(|t| !t.is_empty())
    {
        name = crate::feishu::card::clean_session_label(&t);
    }
    let id_tail: String = session_id
        .strip_prefix("ses_")
        .unwrap_or(&session_id)
        .chars()
        .take(7)
        .collect();
    if name.is_empty() || name == prompt_preview {
        // No echo of the current prompt, but still identify the session.
        id_tail
    } else {
        format!("{} · {}", name, id_tail)
    }
}

/// Refresh the streaming card's subtitle from the server's live session
/// title, returning true if it changed (and the card was re-flushed).
///
/// OpenCode/OpenChamber automatically summarize and rename a session after a
/// turn; cola's card subtitle is only captured when the prompt starts, so it
/// would otherwise stay on the "new session" default title until restart.
/// Called periodically from the render poll loop, so the title follows the
/// server within a poll interval.
async fn refresh_session_title(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
) -> bool {
    // Only meaningful while a turn is actively streaming on a live card.
    let acc_present = cards.cards.lock().await.contains_key(session_id);
    if !acc_present {
        return false;
    }
    let thread_key = sessions.thread_for_session(session_id).await;
    let Some(thread_key) = thread_key else {
        return false;
    };
    // The subtitle is formatted with the session id-tail; recompute it now
    // and keep whatever the server currently reports.
    let fresh = session_subtitle(sessions, backend, &thread_key, "").await;
    if fresh.is_empty() {
        return false;
    }
    let mut live = cards.cards.lock().await;
    let Some(card) = live.get_mut(session_id) else {
        return false;
    };
    if card.acc.title == fresh {
        return false;
    }
    tracing::info!(
        "session {} title updated: {:?} -> {:?}",
        session_id,
        card.acc.title,
        fresh
    );
    card.acc.title = fresh;
    drop(live);
    Turn::flush_card(cards, session_id).await;
    // The topic cover card is the chat-list topic entry — sync it the MOMENT
    // the server title changes mid-turn (not only at turn end), so the list
    // entry updates as early as the title agent finishes (ADR-0023). Only
    // fires on this change tick; the per-tick cost is unchanged.
    crate::bridge::topic::sync_topic_cover_title(cards, sessions, backend, session_id).await;
    true
}

/// Whether `part` would render into `acc`: the dedup half of [`render_part`],
/// so the content-diff probe (Session Sync's "did the chain miss anything")
/// reads exactly the rule the rendering applies — without rendering, and
/// without cloning the accumulator. The apply half re-checks nothing and
/// assumes this returned true.
fn renders_part(acc: &StreamAccumulator, part: &Part) -> bool {
    match part {
        // Reasoning/text parts are written with empty text first, then updated
        // with the full content. Only render once they have content, otherwise
        // we'd freeze the placeholder version.
        Part::Text(text) => {
            !text.text.is_empty()
                && !acc
                    .rendered_parts
                    .contains(&RenderedPart::Text(text.text.clone()))
        }
        Part::Reasoning(reasoning) => {
            !reasoning.text.is_empty()
                && !acc
                    .rendered_parts
                    .contains(&RenderedPart::Reasoning(reasoning.text.clone()))
        }
        Part::Tool(call) => {
            // The current panel IS the call's rendered revision: an update
            // (running → completed, a late output, or a todowrite list
            // rewritten with same-length items) differs from it and
            // re-renders; an unchanged poll skips. A todowrite's clock also
            // participates: each re-sent list stamps `todo_shown_at`, exactly
            // as the state-content signature used to.
            //
            // The comparison is against the typed call BEFORE the panel is
            // built: an unchanged poll (the common case) must not deep-copy
            // the call's raw payloads into a panel only to drop it.
            if call.identity.name == "todowrite" {
                !(acc.todo_panel.as_ref().is_some_and(|panel| panel.call() == call)
                    && acc.todo_shown_at == call.started_at)
            } else {
                !acc.tools
                    .get(&call.identity.call_id)
                    .is_some_and(|panel| panel.call() == call)
            }
        }
        // Step boundaries, patches and part kinds this build does not model
        // render nothing — there is no content to add or dedupe.
        Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::Other(_) => false,
    }
}

/// Render one typed part into the accumulator, applying the dedup rules
/// ([`renders_part`]): text and reasoning are tracked by their content
/// (OpenCode part payloads carry NO `id`, AGENTS.md #9), and a tool call
/// re-renders exactly when its typed panel revision changed. Returns true when
/// the part rendered (not skipped as duplicate/empty).
fn render_part(acc: &mut StreamAccumulator, part: &Part) -> bool {
    if !renders_part(acc, part) {
        return false;
    }
    match part {
        Part::Text(text) => {
            acc.rendered_parts.insert(RenderedPart::Text(text.text.clone()));
            acc.push_text_at(text.started_at, &text.text);
            acc.card_state = crate::feishu::card::CardState::Streaming;
        }
        Part::Reasoning(reasoning) => {
            acc.rendered_parts
                .insert(RenderedPart::Reasoning(reasoning.text.clone()));
            acc.push_reasoning_at(reasoning.started_at, &reasoning.text);
            acc.card_state = crate::feishu::card::CardState::Reasoning;
        }
        Part::Tool(call) => {
            // The panel is a view over the typed call; the Platform assembles
            // the output text and status icon from it (ADR-0042, ADR-0053).
            let panel = crate::feishu::card::tool_render::ToolPanel::new(call.clone());
            if call.identity.name == "todowrite" {
                // A live status section, not a transcript row: the latest call
                // replaces the panel the card tail renders (on the live card,
                // so a split can't strand an outdated list). The clock is
                // always reassigned — a payload without a server time shows no
                // clock rather than the previous call's, which would read as a
                // write time this list never had.
                acc.todo_panel = Some(panel);
                acc.todo_shown_at = call.started_at;
            } else {
                acc.push_tool_at(call.started_at, &call.identity.call_id, panel);
            }
            if call.status == ToolStatus::Running {
                acc.card_state = crate::feishu::card::CardState::Streaming;
            }
        }
        Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::Other(_) => return false,
    }
    // card_state / running-tool changes reset the header phase timer.
    acc.refresh_phase();
    true
}

/// Render a batch of typed parts into the accumulator, skipping anything
/// already rendered (same dedup as the poll loop). Returns true if anything
/// new was rendered. Test-only since the async-native Turn reconciles from the
/// settled transcript instead of a prompt response's inline parts (ADR-0056).
#[cfg(test)]
pub(super) fn render_parts(acc: &mut StreamAccumulator, parts: &[Part]) -> bool {
    let mut rendered_any = false;
    for part in parts {
        if render_part(acc, part) {
            rendered_any = true;
        }
    }
    rendered_any
}

/// Capture the Turn's anchor — the identity of the user message the server
/// stored together with its server time, one fact — matched by the `msg_cola_`
/// id cola chose (ADR-0026). External renders arm with the anchor directly;
/// this fills it in for cola's own turns on the first poll that sees the
/// message. It must run before any filtering: the anchor alone decides which
/// messages are this turn's, and cola's clock cannot. Shared with the
/// post-prompt drain (ADR-0043), whose Backend snapshot must capture the
/// anchor before it can judge an unanswered supplement.
pub(super) fn capture_turn_anchor(acc: &mut StreamAccumulator, transcript: &SessionTranscript) {
    if acc.turn_anchor.is_some() {
        return;
    }
    let Some(cola_message_id) = acc.cola_message_id.as_deref() else {
        return;
    };
    // `anchor_of_user` keeps identity and server time together; a message with
    // no server time cannot anchor (and is retried on the next poll).
    acc.turn_anchor = transcript.anchor_of_user(cola_message_id);
}

/// Capture one assistant message's model facts into the card footer: the
/// answering model, and — when the decoder reports one — its variant. V2's
/// message model ref carries the variant (the session's selection at the time
/// the message ran); V1's decoder deliberately reports none, so V1 keeps its
/// turn-time capture (ADR-0019). Both the render poll and the final reconcile
/// go through here, so the two cannot drift.
pub(super) fn capture_footer_model(acc: &mut StreamAccumulator, message: &crate::backend::TranscriptMessage) {
    let Some(model) = &message.model else {
        return;
    };
    acc.model_id = Some(model.model_id.clone());
    // The decoder reports an absent provider as an empty string; an empty
    // value leaves the last known provider in place.
    if !model.provider_id.is_empty() {
        acc.provider_id = Some(model.provider_id.clone());
    }
    if let Some(variant) = &model.variant {
        acc.variant = Some(variant.clone());
    }
}

/// The completion entry a shell/subagent Wake leaves on the card that hosted
/// its task (ADR-0060): the retired task's kind and label (the Wake's own tag
/// attribute), its identity, and the run's own server-time span. `None` for a
/// Wake that is not a shell/subagent completion — a restart notice or an
/// interruption continuation keeps today's behavior — and the label is never
/// invented: a Wake that named none renders the bare completion line.
///
/// The retired task is joined back from the read through the backend's own
/// correlation ([`Wake::retires`] over each part's derived task), so the entry
/// can only name a run the Wake actually completed; the launch's start time
/// gives the fold's duration. A read that no longer carries the launch renders
/// the entry without a duration rather than inventing one. `finished_at` is the
/// Wake's server time — the caller's own, already checked for orderability, so
/// the entry's clock is never guessed here.
fn wake_completion_entry(
    wake: &Wake,
    transcript: &SessionTranscript,
    finished_at: i64,
) -> Option<TaskCompletionEntry> {
    let kind = match wake.source {
        WakeSource::Shell => TaskKind::Shell,
        WakeSource::Subagent => TaskKind::Subagent,
        WakeSource::Restart | WakeSource::Interrupt | WakeSource::Other(_) | WakeSource::Unknown => {
            return None;
        }
    };
    let retired = transcript
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::Tool(call) => call.background_task(),
            _ => None,
        })
        .find(|task| wake.retires(task));
    // The retired task's own identity is the read's fact; the Wake's named keys
    // are the fallback when the launch is no longer in the read. The launch
    // call's own id covers a task that recorded no shell/child id. Never a
    // made-up one — and never a Wake key that names a different run when the
    // read's task is right there.
    let id = match &retired {
        Some(task) => match kind {
            TaskKind::Shell => task.shell_id.clone().or_else(|| Some(task.tool.call_id.clone())),
            TaskKind::Subagent => task.child_id.clone().or_else(|| Some(task.tool.call_id.clone())),
        },
        None => match kind {
            TaskKind::Shell => wake.shell_id.clone().or_else(|| wake.job_id.clone()),
            TaskKind::Subagent => wake.child_id.clone(),
        },
    };
    Some(TaskCompletionEntry {
        kind,
        label: wake.label.clone(),
        id,
        started_at: retired.as_ref().and_then(|task| task.started_at),
        finished_at: Some(finished_at),
        ending: TaskEnding::Wake {
            state: wake.state.clone(),
        },
    })
}

/// Insert the completion entry for every Wake whose resumed work this render is
/// about to show, and report whether any was inserted (so an entry reaches the
/// card even when no part changed). A Wake that opened the card itself was
/// already announced by its 承接 line, a Wake with no server time cannot be
/// ordered (so it is skipped before its entry is built — the guard supplies
/// `created_ms`), and a Wake outside this card's Turn is not this render's
/// content — all are skipped. Each Wake marks at most once per chain
/// ([`StreamAccumulator::announce_wake`]), so a repeated poll never doubles an
/// entry.
fn render_wake_entries(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    anchor: &TurnAnchor,
) -> bool {
    let mut inserted = false;
    for wake in &transcript.wakes {
        let Some(created_ms) = wake.created_ms else {
            continue;
        };
        if created_ms < anchor.created_ms {
            continue;
        }
        let Some(entry) = wake_completion_entry(wake, transcript, created_ms) else {
            continue;
        };
        if !acc.announce_wake(wake.id.as_str(), created_ms) {
            continue;
        }
        // Keyed at the Wake's moment, so the entry sorts before the work it
        // announces, whose server times are at/after it (the 承接 line's own
        // lesson).
        acc.push_ledger_entry_at(Some(created_ms.saturating_sub(1)), entry);
        inserted = true;
    }
    inserted
}

/// Insert the completion entry for every Background Task a runtime
/// reconciliation retired without a Wake (issue #454) and report whether any
/// was inserted. The retired task has already left the live list, so this entry
/// is its one record when no Wake will ever arrive: the runtime's own
/// completion time when it reported one (the `Lost` ending reports none), the
/// task's launch and identity from the read, and the label joined from the
/// originating tool part's input by `call_id` (the live row's own join).
///
/// The entry renders on the chain that OBSERVED the retirement, whatever
/// anchor that chain has: the launch record never flips, so the observing chain
/// can be a later Turn — or, after a restart, the first chain on a store whose
/// launch card was already collected — and scoping the entry to the launching
/// Turn would silently swallow it (found in review on a real reboot,
/// 2026-10-01: the card settled ✅ with no entry). Exactly-once does not come
/// from the anchor: the read that carries a retirement records it in the
/// backend's process-local overlay, so no later read derives it again, and the
/// Wake announcement set (keyed by a synthetic `runtime:<call_id>` id) covers
/// the reads within the observing chain.
fn render_runtime_entries(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    anchor: &TurnAnchor,
) -> bool {
    let mut inserted = false;
    for retirement in &transcript.runtime_retired {
        let task = &retirement.task;
        let key = format!("runtime:{}", task.tool.call_id);
        let at = retirement
            .finished_at
            .or(task.started_at)
            .unwrap_or(anchor.created_ms);
        if !acc.announce_wake(&key, at) {
            continue;
        }
        let kind = super::state::task_kind(&task.tool.name);
        let ending = match &retirement.ending {
            TaskRetirementEnding::Ended(_) => TaskEnding::RuntimeEnded,
            TaskRetirementEnding::Lost => TaskEnding::Lost,
        };
        let input = transcript.tool_input(task.tool.call_id.as_str());
        let id = match kind {
            TaskKind::Shell => task.shell_id.clone().or_else(|| Some(task.tool.call_id.clone())),
            TaskKind::Subagent => task.child_id.clone().or_else(|| Some(task.tool.call_id.clone())),
        };
        let entry = TaskCompletionEntry {
            kind,
            label: super::state::task_label(kind, input),
            id,
            started_at: task.started_at,
            finished_at: retirement.finished_at,
            ending,
        };
        acc.push_ledger_entry_at(retirement.finished_at, entry);
        inserted = true;
    }
    inserted
}

/// The ledger facts a transcript read owes a card (ADR-0060): the read's
/// remaining live list — so a retired task's row leaves and the still-running
/// ones stay, on a continuation's very first payload or on a yielded card's
/// in-place refresh — and each retiring shell/subagent Wake's completion entry,
/// keyed where the completion happened, so the entry stays on the card that
/// hosted the task. A runtime reconciliation's retirements (issue #454) render
/// their entries through the same site. Both enter the accumulator through the
/// same one-site primitives the live render uses
/// ([`set_ledger_from_read`](StreamAccumulator::set_ledger_from_read),
/// [`render_wake_entries`], [`render_runtime_entries`]), so no path can drift
/// from it. `activities` is the child liveness this read gathered for the
/// ledger's live subagents, keyed by call id (spec #501) — empty on the paths
/// that gather nothing, where a stored fragment is kept rather than dropped.
/// `now_ms` is the read's clock; `cadence` is the granularity its ledger clock
/// is compared at (the live render and its Wake handover at whole minutes, the
/// yielded refresh at whole seconds — [`LedgerCadence`]).
///
/// A Wake handover calls this on the OUTGOING card before its chain splits;
/// Session Sync's in-place pass calls it on a yielded card. Returns whether the
/// card changed at all: a terminal card that did still owes its handover PATCH,
/// while one the read did not touch keeps the ending it shows, and a yielded
/// card is only PATCHed for a real change. `anchor` scopes the completion
/// entries; without one the live list still moves, but a Wake's entry cannot
/// be placed.
pub(super) fn apply_ledger_read(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    activities: &std::collections::HashMap<String, TaskLiveness>,
    anchor: Option<&TurnAnchor>,
    now_ms: i64,
    cadence: LedgerCadence,
) -> bool {
    let mut changed = acc
        .set_ledger_from_read(transcript, activities, now_ms, cadence)
        .owes();
    if let Some(anchor) = anchor {
        changed |= render_wake_entries(acc, transcript, anchor);
        changed |= render_runtime_entries(acc, transcript, anchor);
    }
    changed
}

/// Render the parts of this turn's assistant messages that haven't been
/// rendered yet, and refresh the live Background Task ledger from the read
/// with NO gathered child liveness (the sync callers — tests, the turn-end
/// reconcile — have no gather): every `subagent` row keeps the last fragment
/// that established one. The live render's own entry is [`render_turn_parts`],
/// whose caller applies the ledger from the render's shared gather.
/// Returns true if anything new was rendered.
pub(super) fn render_new_turn_parts(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    let rendered = render_turn_parts(acc, transcript);
    rendered | refresh_ledger(acc, transcript, &std::collections::HashMap::new()).owes()
}

/// Render the parts of this turn's assistant messages that haven't been
/// rendered yet (the live Background Task ledger is the caller's: the render's
/// shared gather has to run between the parts and the ledger). Returns true if
/// anything new was rendered.
///
/// Turn membership is the Session Transcript's shared `turn_for_user`
/// projection, anchored on the Turn's own anchor — the SERVER's message
/// identity and time for this turn (#190). Filtering against cola's submit
/// clock instead dropped the new turn's parts when the server ran behind cola,
/// and bled the previous turn's parts into the new card when it ran ahead.
/// Until the anchor is observed none of the Turn's OWN content renders: with
/// two skewed clocks there is no threshold that tells the two turns apart.
/// The carried calls are the exception — call identity, not time, resolves
/// them — so they reconcile before the gate ([`reconcile_carried_calls`]).
///
/// A message still in flight (no server completion stamp) is rendered while
/// it can still be producing: one created within the turn always is, and one
/// the previous run left streaming when this turn began stays live content
/// while its newest activity is recent (#310) — an orphan the server was
/// killed in goes quiet past the transcript's staleness window and stops
/// belonging, so it cannot replay its parts into every later turn (#378).
/// A completed message belongs to this turn when it finished at/after
/// the anchor — either created within the turn or still being produced as the
/// turn began — while one that finished before the anchor stays the previous
/// turn's and never bleeds in (#190).
fn render_turn_parts(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    capture_turn_anchor(acc, transcript);
    // Carried Tool Panels (ADR-0068) reconcile on EVERY render read, before
    // the anchor gate below: a carried call is resolved by call identity
    // against the whole read, so it needs no Turn anchor. In the canonical
    // restart window the fresh Turn's message is queued behind the still
    // running orphan run, leaving the anchor unobserved for as long as that
    // run lasts — gating the carry on it would freeze the panel at its
    // takeover status for exactly that window.
    let mut rendered_any = reconcile_carried_calls(acc, transcript);
    let Some(anchor) = acc.turn_anchor.clone() else {
        return rendered_any;
    };
    // A merged Wake's completion entry is written before its work renders, so
    // the entry sorts above the parts it announces.
    rendered_any |= render_wake_entries(acc, transcript, &anchor);
    for message in transcript.turn_for_user(&anchor).messages {
        // An error-card retry carries the failed attempt's baseline (#387):
        // its messages stay suppressed, so the rebuilt card streams only the
        // new attempt instead of replaying the old window. Every message this
        // attempt examines is recorded, so a later retry can suppress it too.
        if acc.baseline.suppressed.contains(message.id.as_str()) {
            continue;
        }
        acc.baseline.observed.insert(message.id.as_str().to_string());
        // Capture the answering model + token usage for the card footer.
        capture_footer_model(acc, message);
        // An in-flight step is its own assistant message and carries all-zero
        // usage until it finishes. Capturing that zero would wipe the last
        // completed step's figure and hide the footer's 📊 segment mid-turn.
        if let Some(tokens) = &message.tokens {
            let used = tokens.context_used();
            if used > 0 {
                acc.context_tokens = used;
            }
        }
        for part in &message.parts {
            // A carried call the Turn's own window now renders is the Turn's own
            // live panel again: it leaves the display-only carry set, so the
            // ordinary rules (the #284 live-panel guard included) apply to it
            // exactly as they did before the carry existed.
            if let Part::Tool(call) = part {
                acc.carried_calls.remove(&call.identity.call_id);
            }
            if render_part(acc, part) {
                rendered_any = true;
            }
        }
    }
    rendered_any
}

/// Carried Tool Panels (ADR-0068): a restart takeover seeds the orphaned
/// Turn's running calls; every render read resolves each carried identity
/// against the WHOLE read — PAST the Turn window, whose membership would drop
/// the long-running call's message — so the panel keeps the transcript's
/// current status and output, and its settlement joins the timeline at the
/// server start key it was born with (ADR-0045). It runs on every read, before
/// the anchor gate: the carry is display-only and identity, not time, resolves
/// it, so it works while the fresh Turn's own message is still queued behind a
/// busy orphan run. The ordinary tool dedup applies, so a call the window ALSO
/// renders is not duplicated and a repeated read of an unchanged call is
/// skipped. Returns true when any carried call rendered.
///
/// The identity is live-only: once the transcript settles it the panel is an
/// ordinary timeline record, so it leaves the carried set and the end-of-turn
/// omission (`build_card_inner`, the `has_live_tools` guard) no longer sees
/// it. A carried call still running when the Turn ends never outlives it: the
/// settled card omits it, because no renderer will ever update that `⏳` again.
fn reconcile_carried_calls(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    let mut rendered = false;
    let carried: Vec<String> = acc.carried_calls.iter().cloned().collect();
    for call_id in carried {
        if let Some(call) = transcript.tool_call(&call_id)
            && render_part(acc, &Part::Tool(call.clone()))
        {
            rendered = true;
        }
        if acc.tools.get(&call_id).is_some_and(|panel| !panel.is_live()) {
            acc.carried_calls.remove(&call_id);
        }
    }
    rendered
}

/// Apply one read's live Background Task Ledger to `acc` (ADR-0060), under the
/// same anchor gate the parts render uses: until the Turn's server anchor is
/// observed nothing renders, the ledger included. `activities` is the render's
/// shared child gather (spec #501, ticket #504), keyed by call id, or an empty
/// map on a caller that gathers nothing — then each row keeps its stored
/// fragment. The transcript read is the ledger's authority: a membership change
/// — or a rendered number (a shell row's elapsed, a fragment's age) crossing a
/// whole minute — owes a flush even
/// when no part moved. The clock is compared at the live path's whole-minute
/// cadence on purpose: this loop flushes on content, and a per-render second
/// clock would be churn. The decision's clock is this read's own; the card
/// renders the rows from its build clock, the same second. A fresh fragment is
/// a rendered change that owes its flush, while a gather that established
/// nothing keeps each row's stored fragment growing truthfully. Returns the
/// split decision ([`super::state::LedgerChange`]): a caller that only flushes
/// reads `owes`, while one telling new work from clock churn reads `rows`
/// (#457).
fn refresh_ledger(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    activities: &std::collections::HashMap<String, TaskLiveness>,
) -> super::state::LedgerChange {
    if acc.turn_anchor.is_none() {
        return super::state::LedgerChange::default();
    }
    acc.set_ledger_from_read(
        transcript,
        activities,
        chrono::Utc::now().timestamp_millis(),
        LedgerCadence::Minute,
    )
}

/// Whether rendering `transcript` scoped at `anchor` would add ANY part to
/// `acc` — the content diff Session Sync's Wake step decides on (ADR-0059):
/// the chain has missed something iff this is true. Reads the accumulator's
/// dedup state through the one renderability rule ([`renders_part`]); never
/// mutates it, so the caller does not build a throwaway clone of the card's
/// accumulator on every poll.
///
/// "Any part renders" is equivalent to running the real render: a part that
/// the real pass skips can only have been deduped by an EARLIER part with the
/// same content or revision, and that earlier part was itself renderable — so
/// the first renderable part this scan finds is exactly where a real render
/// would produce something.
pub(super) fn renders_new_content(
    acc: &StreamAccumulator,
    transcript: &SessionTranscript,
    anchor: &TurnAnchor,
) -> bool {
    transcript.turn_for_user(anchor).messages.iter().any(|message| {
        !acc.baseline.suppressed.contains(message.id.as_str())
            && message.parts.iter().any(|part| renders_part(acc, part))
    })
}

/// What one render pass did, read by the loops that drive it: the statistics
/// both log, plus whether the pass produced observable progress — the external
/// renderer's signal for its idle bound (#457).
pub(crate) struct RenderStats {
    /// Parts appended to the card this pass (text/reasoning chunks).
    pub(crate) new_parts: usize,
    /// The card's cumulative text length after the pass (logging).
    pub(crate) text_len: usize,
    /// The card's cumulative reasoning length after the pass (logging).
    pub(crate) reasoning_len: usize,
    /// Whether the pass produced observable progress: rendered content, a tool
    /// panel revision, a live task fragment change, a ledger row's visible
    /// facts moving, or a context-token update — everything the pass flushes
    /// for EXCEPT the per-second header timer and a rendered elapsed/age
    /// crossing its cadence (clock churn is not new work). The external
    /// renderer renews its idle bound on this; header-timer churn alone must
    /// never renew it, or the bound could never fire.
    pub(crate) progressed: bool,
}

/// Render the session's transcript into the streaming card and flush it when
/// something changed. The shared heart of both render loops — `render_poll_loop`
/// (cola's own prompts) and the external-message renderer
/// (`bridge::external`) — so the two never drift apart.
///
/// Returns `Some(stats)` when the accumulator is still present; `None` when it
/// vanished (the caller should stop). [`RenderStats::progressed`] is the
/// external renderer's progress signal: a poll that made no observable
/// progress does not renew its idle bound.
pub(super) async fn render_and_flush(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    requests: &RequestsHandle,
    session_id: &str,
    transcript: &SessionTranscript,
) -> Option<RenderStats> {
    // OpenCode auto-renames sessions after a turn; follow the server's live
    // title so the card subtitle doesn't stay on the "new session" default.
    refresh_session_title(cards, sessions, backend, session_id).await;
    let (changed, header_changed, new_parts, text_len, reasoning_len, anchor) = {
        let mut live = cards.cards.lock().await;
        let card = live.get_mut(session_id)?;
        let before = card.acc.rendered_parts.len();
        let changed = render_turn_parts(&mut card.acc, transcript);
        // The Turn anchor this render captured (or already carried) plus the
        // card it belongs to: the durable live-card record's anchor is written
        // below, outside the lock (ADR-0063).
        let anchor = card.card_message_id.clone().zip(card.acc.turn_anchor.clone());
        // Re-flush when the header changed even without new content: the
        // progress timer keeps ticking, so an idle turn still proves it is
        // alive (ADR-0014). Whole-second timestamps bound this to at most one
        // flush per second.
        let sig = card.acc.header_sig();
        let header_changed = sig != card.last_header_sig;
        if header_changed {
            card.last_header_sig = sig;
        }
        (
            changed,
            header_changed,
            card.acc.rendered_parts.len() - before,
            card.acc.text.len(),
            card.acc.reasoning.len(),
            anchor,
        )
    };
    // Persist the captured anchor on the durable record (ADR-0063), so a
    // restart's reap can ask the transcript what became of this Turn's message
    // instead of probing for it. A no-op while the record names another card.
    if let Some((card_message_id, anchor)) = &anchor {
        cards.live_cards.set_anchor(session_id, card_message_id, anchor);
    }
    // Keep the footer's context segment current (ADR-0044): the token usage
    // landed in the render above, and the window lookup is memoized per
    // (provider, model) for the turn, so later polls are a field swap. Runs
    // outside the cards lock (network).
    crate::bridge::turn::state::refresh_context_window(cards, backend, session_id).await;
    // A usage (or window) change must flush even when no part and no header
    // second changed: the 📊 segment is footer state the header signature
    // cannot see, and a silently-stale percentage is the bug this fixes.
    let context_changed = {
        let mut live = cards.cards.lock().await;
        match live.get_mut(session_id) {
            Some(card) => {
                let sig = card.acc.context_sig();
                let changed = card.last_context_sig != sig;
                card.last_context_sig = sig;
                changed
            }
            None => false,
        }
    };
    // The live child liveness this render needs, gathered ONCE for both
    // surfaces that show it (spec #501, ticket #504): the card's live task
    // panels and the read's live background subagents. It runs AFTER the parts
    // render, so a panel that first appeared this poll is in the union; the
    // union means a child named by both is read — and its wait queried — at
    // most once this render, while a render that names no child at all (V1, a
    // session with no live task) spends no request. The fragment's age is
    // measured at the build clock.
    let (panel_children, liveness) =
        gather_render_liveness(cards, backend, requests, session_id, transcript).await;
    // Apply that ONE gather to both surfaces under one lock: the ledger's
    // `subagent` rows and every live task panel (ADR-0060/0054). A change on
    // either must flush even when no part, header second or context figure
    // moved — the line is the only thing that changed.
    let (ledger, liveness_changed) = {
        let mut live = cards.cards.lock().await;
        match live.get_mut(session_id) {
            Some(card) => (
                refresh_ledger(&mut card.acc, transcript, &liveness),
                apply_task_liveness(&mut card.acc, &panel_children, &liveness),
            ),
            None => (super::state::LedgerChange::default(), false),
        }
    };
    // Observable progress: everything this pass flushes for except the
    // per-second header timer, which ticks on an idle run by design. The
    // external renderer's idle bound renews on this and must not read the
    // header second as progress (#457). The ledger contributes only its ROW
    // half — a rendered elapsed/age crossing its cadence is clock churn, not
    // new work, and renewing on it would keep a silent task's card live
    // forever.
    let progressed = changed || context_changed || ledger.rows || liveness_changed;
    if progressed || header_changed || ledger.clock {
        Turn::flush_card(cards, session_id).await;
    }
    Some(RenderStats {
        new_parts,
        text_len,
        reasoning_len,
        progressed,
    })
}

/// A child session's liveness as its transcript reports it (ADR-0054): the
/// timestamp of its newest activity and its newest still-running tool. `None`
/// when the transcript carries no timestamp at all (a child that just started,
/// or a payload without times) — no line is better than a made-up age.
fn child_liveness(transcript: &SessionTranscript) -> Option<TaskLiveness> {
    let mut newest_ms: Option<i64> = None;
    let mut observe = |at: Option<i64>| {
        if let Some(at) = at {
            newest_ms = Some(newest_ms.map_or(at, |current: i64| current.max(at)));
        }
    };
    // The newest live tool by server start time (an untimed call only wins
    // when nothing timed is running), and the newest part for the phase when
    // nothing is live.
    let mut current: Option<(i64, String, Option<i64>)> = None;
    let mut newest_part: Option<&Part> = None;
    for message in &transcript.messages {
        if let Some(time) = &message.time {
            observe(Some(time.completed.unwrap_or(time.created)));
        }
        for part in &message.parts {
            newest_part = Some(part);
            match part {
                Part::Tool(call) => {
                    observe(call.started_at);
                    if call.status.is_live() {
                        let at = call.started_at.unwrap_or(i64::MIN);
                        if current.as_ref().is_none_or(|(best, _, _)| at >= *best) {
                            current = Some((at, call.identity.name.clone(), call.started_at));
                        }
                    }
                }
                Part::Text(_) | Part::Reasoning(_) => observe(part.started_at()),
                Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::Other(_) => {}
            }
        }
    }
    let activity = match current {
        Some((_, name, started_at)) => ChildActivity::Tool { name, started_at },
        None => match newest_part {
            Some(Part::Reasoning(_)) => ChildActivity::Reasoning,
            Some(Part::Text(_)) => ChildActivity::Replying,
            _ => ChildActivity::Thinking,
        },
    };
    Some(TaskLiveness {
        activity,
        last_activity_ms: newest_ms?,
        wait: None,
    })
}

/// One batched gather of child-session liveness (ADR-0054, spec #501): every
/// distinct child in `children` costs exactly ONE transcript light read and,
/// on a successful read, one pending-wait query; every `(call_id, child)` pair
/// names its child's liveness in the returned map under its own call id (one
/// child can be driven by several calls, and each renders the same state). A
/// child whose read fails — or whose transcript carries no timestamp — is
/// absent from the map, so its caller keeps whatever fragment it last rendered
/// instead of inventing one; a child named by several pairs is still read once.
pub(super) async fn gather_child_liveness(
    backend: &Arc<dyn crate::backend::Backend>,
    requests: &RequestsHandle,
    children: &[(String, String)],
) -> std::collections::HashMap<String, TaskLiveness> {
    let mut by_child: std::collections::HashMap<&str, Option<TaskLiveness>> =
        std::collections::HashMap::new();
    let mut gathered = std::collections::HashMap::new();
    for (call_id, child) in children {
        // The distinct-child cache holds failures too (`None`), so a child
        // whose read already failed is not read again for another call.
        if !by_child.contains_key(child.as_str()) {
            let liveness = match backend.transcript(child).await {
                Ok(transcript) => match child_liveness(&transcript) {
                    Some(mut liveness) => {
                        liveness.wait = requests.wait_for(child).await;
                        Some(liveness)
                    }
                    None => None,
                },
                Err(_) => None,
            };
            by_child.insert(child.as_str(), liveness);
        }
        if let Some(liveness) = by_child.get(child.as_str()).and_then(Option::as_ref) {
            gathered.insert(call_id.clone(), liveness.clone());
        }
    }
    gathered
}

/// The live background subagents a transcript read names, as the
/// `(call_id, child_session_id)` pairs the shared liveness gather consumes
/// (spec #501): a `subagent` Background Task that recorded a child, minus the
/// rows a runtime reconciliation marked unconfirmed — an unconfirmed row
/// renders no activity, so its child is never read. A shell task has no child.
pub(super) fn background_subagent_children(transcript: &SessionTranscript) -> Vec<(String, String)> {
    transcript
        .background_tasks
        .iter()
        .filter(|task| {
            super::state::task_kind(&task.tool.name) == TaskKind::Subagent
                && !transcript.unconfirmed_tasks.contains(task.tool.call_id.as_str())
        })
        .filter_map(|task| {
            task.child_id
                .as_deref()
                .map(|child| (task.tool.call_id.clone(), child.to_string()))
        })
        .collect()
}

/// The card's live `task`/`subagent` panels' `(call id, child Session id)`
/// pairs (ADR-0054), snapshotted under a short lock: the gather they feed is
/// network and never runs under it.
async fn live_panel_children(cards: &CardsHandle, session_id: &str) -> Vec<(String, String)> {
    let live = cards.cards.lock().await;
    match live.get(session_id) {
        Some(card) => card.acc.live_task_children(),
        None => Vec::new(),
    }
}

/// The child liveness one live render needs, gathered ONCE for both surfaces
/// that show it (spec #501, ticket #504): the union of the card's live task
/// panels' children ([`live_panel_children`]) and the read's live background
/// subagents ([`background_subagent_children`]). One transcript light read and
/// one pending-wait query per distinct child, so a child named by both a panel
/// and a ledger row is read once; a render that names no child at all (V1, or
/// a session with no live task) spends no request. Returns the panel pairs the
/// caller attaches the result to, alongside the map keyed by every pair's call
/// id.
async fn gather_render_liveness(
    cards: &CardsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    requests: &RequestsHandle,
    session_id: &str,
    transcript: &SessionTranscript,
) -> (
    Vec<(String, String)>,
    std::collections::HashMap<String, TaskLiveness>,
) {
    let panels = live_panel_children(cards, session_id).await;
    let mut children = panels.clone();
    children.extend(background_subagent_children(transcript));
    if children.is_empty() {
        return (panels, std::collections::HashMap::new());
    }
    (panels, gather_child_liveness(backend, requests, &children).await)
}

/// Attach the render's shared gather to every live `task` panel in `acc`
/// (ADR-0054). Returns true when a panel changed. A panel the gather could not
/// establish keeps its previous line (its stored activity time keeps the age
/// growing truthfully) instead of clearing it or inventing one.
fn apply_task_liveness(
    acc: &mut StreamAccumulator,
    panels: &[(String, String)],
    gathered: &std::collections::HashMap<String, TaskLiveness>,
) -> bool {
    let mut changed = false;
    for (call_id, _) in panels {
        if let Some(liveness) = gathered.get(call_id).cloned() {
            changed |= acc.set_tool_liveness(call_id, Some(liveness));
        }
    }
    changed
}

/// Incremental renderer: while the submitted prompt is being admitted, poll
/// the session's transcript and flush the card as parts complete (reasoning,
/// tools, text). `done` stops the loop once the submit returns; the
/// post-prompt drain then owns the observation. `poll_ms` is the injected
/// cadence (`TurnConfig::turn_render_poll_ms`), so tests never wait on the
/// production 1.5 s.
async fn render_poll_loop(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    requests: &RequestsHandle,
    session_id: String,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    poll_ms: u64,
) {
    use std::sync::atomic::Ordering;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(poll_ms)).await;
        if done.load(Ordering::SeqCst) {
            return;
        }
        let transcript = match backend.transcript(&session_id).await {
            Ok(transcript) => transcript,
            Err(e) => {
                tracing::warn!("render poll transcript: {}", e);
                continue;
            }
        };
        match render_and_flush(cards, sessions, backend, requests, &session_id, &transcript).await {
            // Accumulator gone (turn completed and was cleaned up); keep polling
            // until the prompt returns so late parts are still caught.
            None => continue,
            Some(stats) => {
                if stats.new_parts > 0 {
                    tracing::info!(
                        "render poll: {} new parts, text={} reasoning={}",
                        stats.new_parts,
                        stats.text_len,
                        stats.reasoning_len
                    );
                }
            }
        }
    }
}

/// One attempt's incremental renderer: the poll loop plus its stop flag. Owns
/// the spawn/stop pairing so an attempt cannot leak a running poll loop.
pub(super) struct RenderPoll {
    done: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl RenderPoll {
    pub(super) fn spawn(handles: &TurnHandles, session_id: &str, thread_key: &ThreadKey) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let cards = handles.cards.clone();
        let sessions = handles.sessions.clone();
        let backend = Arc::clone(&handles.backend);
        let requests = handles.requests.clone();
        let sid = session_id.to_string();
        let flag = Arc::clone(&done);
        let poll_ms = handles.config.render_poll_ms();
        // A spawn does not inherit the turn's span — the poll runs on its own
        // task — so it is instrumented explicitly with the same fields: its
        // lines must keep the session (ADR-0048). Rooted, because the ambient
        // parent here is the turn's span.
        let span = span::turn(session_id, thread_key, None);
        let handle = tokio::spawn(
            async move {
                render_poll_loop(&cards, &sessions, &backend, &requests, sid, flag, poll_ms).await;
            }
            .instrument(span),
        );
        Self { done, handle }
    }

    pub(super) async fn stop(self) {
        self.done.store(true, Ordering::SeqCst);
        let _ = self.handle.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        BackgroundTask, MessageId, MessageRole, MessageTime, ReasoningPart, StepFinish, StepStart, ToolCall,
        ToolIdentity, ToolOutput, TranscriptMessage, TurnAnchor, TurnSettle,
    };
    use crate::bridge::App;
    use crate::bridge::test_support::{
        MockBackend, PlatformCall, RecordingPlatform, build_app, card_text, realistic_parts,
        seed_cover_title, seed_entry, test_config, test_work_dir, text_part, turn_anchor, typed_message,
    };
    use crate::bridge::turn::state::StreamAccumulator;
    use crate::feishu::card::CardState;
    use crate::feishu::card::tool_render::ToolPanel;

    /// A typed assistant message: identity, role, server time and parts, with
    /// no backend field name in the fixture (spec #332).
    fn message(id: &str, created: i64, parts: Vec<Part>) -> TranscriptMessage {
        typed_message(id, MessageRole::Assistant, Some(created), parts)
    }

    /// A typed assistant message with an explicit completion stamp (`None`
    /// models a message still in flight).
    fn message_in_flight(
        id: &str,
        created: i64,
        completed: Option<i64>,
        parts: Vec<Part>,
    ) -> TranscriptMessage {
        TranscriptMessage {
            id: MessageId::new(id),
            role: MessageRole::Assistant,
            time: Some(MessageTime { created, completed }),
            model: None,
            tokens: None,
            error: None,
            parts,
        }
    }

    fn reasoning_part(text: &str) -> Part {
        Part::Reasoning(ReasoningPart {
            text: text.to_string(),
            started_at: None,
        })
    }

    fn reasoning_at(text: &str, started_at: i64) -> Part {
        Part::Reasoning(ReasoningPart {
            text: text.to_string(),
            started_at: Some(started_at),
        })
    }

    /// A typed tool call with the given typed output blocks.
    fn tool_call(
        name: &str,
        call_id: &str,
        status: ToolStatus,
        started_at: Option<i64>,
        input: Option<serde_json::Value>,
        output: Vec<crate::backend::ContentBlock>,
    ) -> ToolCall {
        ToolCall {
            identity: ToolIdentity {
                name: name.to_string(),
                call_id: call_id.to_string(),
            },
            status,
            started_at,
            input,
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks: output,
                error: None,
            },
        }
    }

    /// A tool call whose decoded output is one text block.
    fn tool(
        name: &str,
        call_id: &str,
        status: ToolStatus,
        started_at: Option<i64>,
        input: Option<serde_json::Value>,
        output: Option<&str>,
    ) -> Part {
        Part::Tool(tool_call(
            name,
            call_id,
            status,
            started_at,
            input,
            output
                .map(|text| vec![crate::backend::ContentBlock::Text(text.to_string())])
                .unwrap_or_default(),
        ))
    }

    /// A typed `todowrite` call: the list as input, JSON-encoded as the text
    /// output, exactly the shapes the model re-sends whole on every update.
    fn todowrite(call_id: &str, at_ms: i64, todos: serde_json::Value) -> Part {
        Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "todowrite".into(),
                call_id: call_id.into(),
            },
            status: ToolStatus::Completed,
            started_at: Some(at_ms),
            input: Some(serde_json::json!({ "todos": todos.clone() })),
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks: vec![crate::backend::ContentBlock::Text(todos.to_string())],
                error: None,
            },
        })
    }

    #[test]
    fn render_part_marks_content_and_tracks_header_phase() {
        use crate::bridge::turn::state::HeaderPhase;

        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));
        assert_eq!(acc.current_phase, Some(HeaderPhase::Loading));

        let transcript = SessionTranscript::new(vec![message(
            "a1",
            100,
            vec![reasoning_part("Let me think"), text_part("Answer")],
        )]);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(acc.current_phase, Some(HeaderPhase::Streaming));
    }

    /// Every step is its own assistant message, and an in-flight step carries
    /// all-zero usage until it finishes. The zero must not wipe the last
    /// completed step's figure: doing so hid the footer's 📊 segment for the
    /// whole streaming phase and showed it only at turn end (reported bug).
    #[test]
    fn inflight_zero_usage_keeps_the_last_completed_steps_figure() {
        use crate::backend::{ModelIdentity, TokenUsage};

        let message = |id: &str, created: i64, tokens: TokenUsage| TranscriptMessage {
            id: MessageId::new(id),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created,
                completed: Some(created),
            }),
            model: Some(ModelIdentity {
                provider_id: "p".into(),
                model_id: "m".into(),
                variant: None,
            }),
            tokens: Some(tokens),
            error: None,
            parts: Vec::new(),
        };
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        let transcript = SessionTranscript::new(vec![
            message(
                "a1",
                100,
                TokenUsage {
                    total: 210_239,
                    ..Default::default()
                },
            ),
            // The step now streaming: its message exists, usage still zeros.
            message("a2", 200, TokenUsage::default()),
        ]);
        render_new_turn_parts(&mut acc, &transcript);
        assert_eq!(acc.context_tokens, 210_239);

        // The next completed step updates the figure as usual.
        let transcript = SessionTranscript::new(vec![
            message("a2", 200, TokenUsage::default()),
            message(
                "a3",
                300,
                TokenUsage {
                    total: 216_860,
                    ..Default::default()
                },
            ),
        ]);
        render_new_turn_parts(&mut acc, &transcript);
        assert_eq!(acc.context_tokens, 216_860);
    }

    #[test]
    fn render_parts_shows_reasoning_and_tool_output() {
        // A real turn's typed parts: reasoning text, a settled bash call with
        // its output block, and the final answer text.
        let parts = vec![
            Part::StepStart(StepStart),
            reasoning_part("The user is asking in Chinese."),
            tool(
                "bash",
                "call_1",
                ToolStatus::Completed,
                None,
                Some(serde_json::json!({"command": "pwd && ls -la"})),
                Some("/root/workspace/dev/cola\n..."),
            ),
            Part::StepFinish(StepFinish {
                reason: crate::backend::FinishReason::ToolCalls,
            }),
            Part::StepStart(StepStart),
            text_part("我是 opencode。"),
            Part::StepFinish(StepFinish {
                reason: crate::backend::FinishReason::Stop,
            }),
        ];

        let mut acc = StreamAccumulator::new("test");
        render_parts(&mut acc, &parts);
        acc.card_state = CardState::Done;

        assert!(acc.reasoning.contains("The user is asking in Chinese."));
        assert_eq!(acc.tools.len(), 1);
        let tool = &acc.tools["call_1"];
        assert_eq!(tool.name(), "bash");
        assert_eq!(tool.status(), &ToolStatus::Completed);
        assert!(
            tool.output()
                .as_deref()
                .unwrap()
                .contains("/root/workspace/dev/cola")
        );
        assert!(
            tool.input()
                .map(|i| i.to_string())
                .unwrap_or_default()
                .contains("pwd")
        );

        let card = acc.build_card().to_string();
        assert!(card.contains("推理过程"));
        assert!(card.contains("bash"));
    }

    #[test]
    fn render_parts_falls_back_to_metadata_output() {
        // The decoder's text block already carries the `metadata.output`
        // fallback; the renderer just shows it.
        let parts = vec![tool(
            "read",
            "call_2",
            ToolStatus::Completed,
            None,
            Some(serde_json::json!({"path": "src/main.rs"})),
            Some("fn main() {}"),
        )];
        let mut acc = StreamAccumulator::new("test");
        render_parts(&mut acc, &parts);
        assert_eq!(acc.tools["call_2"].output().as_deref(), Some("fn main() {}"));
    }

    /// A tool part's `running` status marks the card Streaming; a completed one
    /// leaves the state alone. The mutation audit (render.rs:291) found the
    /// status comparison surviving the suite.
    #[test]
    fn a_running_tool_marks_the_card_streaming() {
        let running = vec![tool(
            "bash",
            "call_running",
            ToolStatus::Running,
            None,
            Some(serde_json::json!({"command": "sleep 1"})),
            None,
        )];
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        render_parts(&mut acc, &running);
        assert_eq!(
            acc.card_state,
            CardState::Streaming,
            "a running tool must keep the card Streaming"
        );

        let completed = vec![tool(
            "bash",
            "call_done",
            ToolStatus::Completed,
            None,
            None,
            Some("ok"),
        )];
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        render_parts(&mut acc, &completed);
        assert_eq!(
            acc.card_state,
            CardState::Done,
            "a completed tool must not flip the card state"
        );
    }

    /// #202: an `apply_patch` records its real change in its raw metadata (like
    /// `edit`), while the text output is only the success summary. The panel
    /// must show the diff, keep an LSP note as its tail, and drop the summary.
    #[test]
    fn apply_patch_uses_metadata_diff_and_keeps_the_lsp_tail() {
        let diff = "\
Index: /x/src/main.rs
===================================================================
--- /x/src/main.rs
+++ /x/src/main.rs
@@ -1,3 +1,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;";
        let output = "Success. Updated the following files:\nM src/main.rs\n\n\
                      LSP errors detected in src/main.rs, please fix:\nboom";
        let mut call = tool_call(
            "apply_patch",
            "call_patch",
            ToolStatus::Completed,
            None,
            Some(serde_json::json!({"patchText": "*** Begin Patch"})),
            vec![crate::backend::ContentBlock::Text(output.to_string())],
        );
        call.metadata = Some(serde_json::json!({"diff": diff}));
        let parts = vec![Part::Tool(call)];
        let mut acc = StreamAccumulator::new("test");
        render_parts(&mut acc, &parts);
        let out = acc.tools["call_patch"].output().unwrap();
        assert!(out.contains("+let b = 3;"), "hunk shown: {out}");
        assert!(
            out.contains("LSP errors detected in src/main.rs"),
            "LSP tail kept: {out}"
        );
        assert!(
            !out.contains("Success. Updated"),
            "success summary dropped: {out}"
        );
    }

    #[test]
    fn render_new_turn_parts_filters_turn_and_dedups() {
        // The user message's server time is the turn anchor; the old COMPLETED
        // assistant (created 100, finished 150) sits before it, the current one
        // (3000) after.
        let anchor = 2000;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(anchor));

        let transcript = SessionTranscript::new(vec![
            // Old turn assistant message (completed before the anchor) — skipped.
            message_in_flight("old", 100, Some(150), vec![reasoning_part("old reasoning")]),
            // User message — skipped (not assistant).
            typed_message("user", MessageRole::User, Some(2000), vec![text_part("question")]),
            // Current turn assistant message.
            message(
                "a1",
                3000,
                vec![
                    reasoning_part("Let me think"),
                    tool(
                        "bash",
                        "call_1",
                        ToolStatus::Completed,
                        None,
                        Some(serde_json::json!({"command": "ls"})),
                        Some("src"),
                    ),
                ],
            ),
        ]);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        assert!(acc.reasoning.contains("Let me think"));
        assert_eq!(acc.tools.len(), 1);
        assert_eq!(acc.rendered_parts.len(), 1);
        assert!(!acc.text.contains("question"));
        assert!(!acc.reasoning.contains("old reasoning"));

        assert!(!render_new_turn_parts(&mut acc, &transcript));
    }

    /// #310: a still-running assistant message created BEFORE the turn anchor is
    /// live content, not a previous turn's. The server was mid-step when this
    /// turn's user message landed (typically a long `task` from the previous
    /// Turn, whose guard was released while the session stayed busy — #284).
    /// Filtering it by `created >= anchor` dropped every part it produced and
    /// left the new card blank until finalization.
    #[test]
    fn an_in_flight_message_created_before_the_anchor_renders() {
        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let transcript = SessionTranscript::new(vec![
            // The new turn's own user message: the anchor.
            TranscriptMessage {
                id: MessageId::new("msg_cola_1"),
                role: MessageRole::User,
                time: Some(MessageTime {
                    created: 2000,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![text_part("我的问题你回答了吗")],
            },
            // The previous run's step, still in flight (no completion stamp),
            // created BEFORE the anchor.
            message_in_flight(
                "a_inflight",
                500,
                None,
                vec![
                    reasoning_part("还在研究"),
                    tool(
                        "task",
                        "call_task",
                        ToolStatus::Running,
                        None,
                        Some(serde_json::json!({"description": "research"})),
                        None,
                    ),
                ],
            ),
        ]);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(
            acc.turn_anchor,
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 2000,
            }),
            "the anchor is the message's identity together with its server time"
        );
        assert!(
            acc.reasoning.contains("还在研究"),
            "the in-flight message's reasoning must render live: {:?}",
            acc.reasoning
        );
        assert_eq!(
            acc.tools["call_task"].status(),
            &ToolStatus::Running,
            "the running panel must render live, not only at finalization"
        );

        // Dedup still holds on the next poll.
        assert!(!render_new_turn_parts(&mut acc, &transcript));
    }

    /// #310: once that pre-anchor message completes — the server stamps
    /// `time.completed` at/after the anchor — it is still this turn's live
    /// content, so its final parts (here a settled tool panel) must keep
    /// rendering. Dropping it on completion would hide the outcome until
    /// finalization.
    #[test]
    fn a_message_completed_after_the_anchor_keeps_rendering() {
        let anchor = 2000;
        let mut acc = StreamAccumulator::new("proj");
        acc.turn_anchor = Some(turn_anchor(anchor));
        let transcript = |completed: Option<i64>, status: ToolStatus, output: &str| {
            SessionTranscript::new(vec![message_in_flight(
                "a_prev",
                500,
                completed,
                vec![tool(
                    "task",
                    "call_task",
                    status,
                    None,
                    Some(serde_json::json!({"description": "research"})),
                    Some(output),
                )],
            )])
        };

        // In flight when the anchor lands: rendered (no completion stamp).
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(None, ToolStatus::Running, "")
        ));
        assert_eq!(acc.tools["call_task"].status(), &ToolStatus::Running);

        // Completed AFTER the anchor: the settled panel still renders.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(Some(2_500), ToolStatus::Completed, "research done")
        ));
        assert_eq!(acc.tools["call_task"].status(), &ToolStatus::Completed);
        assert_eq!(acc.tools["call_task"].output().as_deref(), Some("research done"));

        // Re-fetching the same settled state must not duplicate.
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript(Some(2_500), ToolStatus::Completed, "research done")
        ));
    }

    // --- The restart carry (ADR-0068) ---------------------------------------
    //
    // A tool that settled while cola was down is deliberately out of scope
    // here: the faithful no-duplicate/no-omission restore of missed content is
    // #505's question, not a tail carry's (ADR-0068's scope decision).

    /// Regression for the restart carry (ADR-0068): a running call inside the
    /// in-flight window already renders onto the successor through the ordinary
    /// transcript render — no carry needed — and its completion settles it into
    /// the timeline exactly once. The probe that pinned this behavior becomes
    /// the regression the carry must not break.
    #[test]
    fn a_recent_inflight_tool_already_renders_on_the_successor() {
        let anchor = 2_000_000;
        let transcript = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_new",
                    MessageRole::User,
                    Some(anchor),
                    vec![text_part("新的问题")],
                ),
                message_in_flight(
                    "a_inflight",
                    anchor - 60_000,
                    None,
                    vec![tool(
                        "shell",
                        "call_sleep",
                        status,
                        Some(anchor - 30_000),
                        Some(serde_json::json!({"command": "sleep 600"})),
                        output,
                    )],
                ),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_new".into());
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Running, None)
        ));
        assert_eq!(acc.tools["call_sleep"].status(), &ToolStatus::Running);
        let (card, full) = acc.build_card_with_split();
        assert!(!full, "a live card with only a tail panel must not split");
        assert!(
            card_text(&card).contains("⏳ shell"),
            "the running panel must ride the successor's live tail: {card}"
        );

        // The tool completes while its message is still inside the window: the
        // settled panel joins the successor's timeline, exactly once.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, Some("slept"))
        ));
        assert_eq!(acc.tools["call_sleep"].status(), &ToolStatus::Completed);
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("slept").count(), 1, "settled once: {text}");
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, Some("slept"))
        ));
    }

    /// ADR-0068: a call older than the in-flight window is dropped by the
    /// successor's own render, so the takeover carries the orphan Turn's
    /// running call — the successor's live tail shows `⏳`, and its completion
    /// renders exactly once, joining the timeline at its server start key. A
    /// later transcript render of the same call does not duplicate it.
    #[test]
    fn a_stale_inflight_tool_is_carried_and_settles_once_into_the_timeline() {
        let anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: anchor - 31 * 60_000,
        };
        let stale_tool = |status: ToolStatus, output: Option<&str>| {
            vec![tool(
                "shell",
                "call_sleep",
                status,
                Some(anchor - 11 * 60_000),
                Some(serde_json::json!({"command": "sleep 3600"})),
                output,
            )]
        };
        let transcript = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_new",
                    MessageRole::User,
                    Some(anchor),
                    vec![text_part("新的问题")],
                ),
                message_in_flight("a_stale", anchor - 30 * 60_000, None, stale_tool(status, output)),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_new".into());
        // The takeover seeds the orphan Turn's running calls before the prompt.
        assert_eq!(
            acc.carry_tools(&transcript(ToolStatus::Running, None).turn_running_tools(&orphan)),
            1,
            "the orphan's running call is the carry set"
        );
        render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
        assert_eq!(acc.tools["call_sleep"].status(), &ToolStatus::Running);
        let (card, full) = acc.build_card_with_split();
        assert!(!full, "a live card with only a tail panel must not split");
        assert!(
            card_text(&card).contains("⏳ shell"),
            "the carried call rides the successor's live tail: {card}"
        );

        // The tool completes while its message stays stale: the carried
        // identity reconciles it past the Turn window, so the panel leaves the
        // tail and joins the timeline at its server start key — exactly once.
        let settled = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_new",
                MessageRole::User,
                Some(anchor),
                vec![text_part("新的问题")],
            ),
            message_in_flight(
                "a_stale",
                anchor - 30 * 60_000,
                None,
                stale_tool(ToolStatus::Completed, Some("slept")),
            ),
            message("a_new", anchor + 1_000, vec![text_part("新回答")]),
        ]);
        assert!(render_new_turn_parts(&mut acc, &settled));
        assert!(!acc.tools["call_sleep"].is_live());
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("slept").count(), 1, "settled once: {text}");
        assert!(
            text.find("slept") < text.find("新回答"),
            "the settled call joins at its server start key, before later content: {text}"
        );
        assert!(!render_new_turn_parts(&mut acc, &settled));
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(
            text.matches("slept").count(),
            1,
            "a later transcript render does not duplicate the call: {text}"
        );
    }

    /// ADR-0068's carry set is every LIVE call — `pending` as well as
    /// `running` (`ToolStatus::is_live`) — so a pending orphan is seeded into
    /// the successor's live tail exactly like a running one, and its
    /// settlement reconciles exactly once at its server start key.
    #[test]
    fn a_pending_orphan_call_is_carried_and_settles_once() {
        let anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: anchor - 31 * 60_000,
        };
        let stale_tool = |status: ToolStatus, output: Option<&str>| {
            vec![tool(
                "shell",
                "call_sleep",
                status,
                Some(anchor - 11 * 60_000),
                Some(serde_json::json!({"command": "sleep 3600"})),
                output,
            )]
        };
        let transcript = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_new",
                    MessageRole::User,
                    Some(anchor),
                    vec![text_part("新的问题")],
                ),
                message_in_flight("a_stale", anchor - 30 * 60_000, None, stale_tool(status, output)),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_new".into());
        // The takeover seeds the orphan Turn's pending call: `pending` is live,
        // so it is part of ADR-0068's carry set.
        assert_eq!(
            acc.carry_tools(&transcript(ToolStatus::Pending, None).turn_running_tools(&orphan)),
            1,
            "a pending orphan call is carried like a running one"
        );
        assert_eq!(acc.tools["call_sleep"].status(), &ToolStatus::Pending);
        let (card, full) = acc.build_card_with_split();
        assert!(!full, "a live card with only a tail panel must not split");
        assert!(
            card_text(&card).contains("⏳ shell"),
            "the carried pending call rides the successor's live tail: {card}"
        );

        // The call completes while its message stays stale: the carried
        // identity reconciles it past the Turn window, exactly once.
        let settled = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_new",
                MessageRole::User,
                Some(anchor),
                vec![text_part("新的问题")],
            ),
            message_in_flight(
                "a_stale",
                anchor - 30 * 60_000,
                None,
                stale_tool(ToolStatus::Completed, Some("slept")),
            ),
        ]);
        assert!(render_new_turn_parts(&mut acc, &settled));
        assert!(!acc.tools["call_sleep"].is_live());
        assert!(
            acc.carried_calls.is_empty(),
            "a settled carried call leaves the display-only carry set"
        );
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("slept").count(), 1, "settled once: {text}");
        assert!(!render_new_turn_parts(&mut acc, &settled));
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(
            text.matches("slept").count(),
            1,
            "a later transcript render does not duplicate the call: {text}"
        );
    }

    /// The carry is scoped to the orphan Turn's projection: an older, unrelated
    /// turn's stale `running` part is never resurrected on the successor.
    #[test]
    fn only_the_orphan_turns_running_calls_are_carried() {
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: 1_000_000,
        };
        let transcript = SessionTranscript::new(vec![
            // An unrelated older turn's stale running part: created before the
            // orphan's anchor, its newest activity long past the window.
            message_in_flight(
                "a_older",
                1_000_000 - 30 * 60_000,
                None,
                vec![tool(
                    "shell",
                    "call_ghost",
                    ToolStatus::Running,
                    Some(1_000_000 - 29 * 60_000),
                    Some(serde_json::json!({"command": "sleep 9999"})),
                    None,
                )],
            ),
            // The orphan Turn's own still-running call.
            message_in_flight(
                "a_orphan",
                1_000_000 + 5_000,
                None,
                vec![tool(
                    "shell",
                    "call_sleep",
                    ToolStatus::Running,
                    Some(1_000_000 + 5_000),
                    Some(serde_json::json!({"command": "sleep 3600"})),
                    None,
                )],
            ),
        ]);

        let carried: Vec<String> = transcript
            .turn_running_tools(&orphan)
            .iter()
            .map(|call| call.identity.call_id.clone())
            .collect();
        assert_eq!(
            carried,
            ["call_sleep"],
            "only the orphan Turn's running call belongs to the carry set"
        );
    }

    /// A killed run's carried call reconciles to the transcript's final status
    /// on every render read — beyond the Turn window — and never outlives the
    /// Turn: the panel invents no ending while the transcript says `running`,
    /// and the successor's own settle decision is untouched by it. When the
    /// transcript finally records a status, the panel takes it and settles.
    #[test]
    fn a_carried_call_beyond_the_window_reconciles_on_every_render_read() {
        let anchor = 2_000_000;
        let new_anchor = TurnAnchor {
            message_id: MessageId::new("msg_cola_new"),
            created_ms: anchor,
        };
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: anchor - 31 * 60_000,
        };
        let transcript = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_new",
                    MessageRole::User,
                    Some(anchor),
                    vec![text_part("新的问题")],
                ),
                message_in_flight(
                    "a_stale",
                    anchor - 30 * 60_000,
                    None,
                    vec![tool(
                        "shell",
                        "call_sleep",
                        status,
                        Some(anchor - 11 * 60_000),
                        Some(serde_json::json!({"command": "sleep 3600"})),
                        output,
                    )],
                ),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_new".into());
        acc.carry_tools(&transcript(ToolStatus::Running, None).turn_running_tools(&orphan));
        // Repeated reads while the transcript says running: the carried panel
        // stays truthfully live and never invents an ending.
        for _ in 0..3 {
            render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
            assert!(
                acc.tools["call_sleep"].is_live(),
                "a killed run's carried call keeps the transcript's running status"
            );
        }
        assert_eq!(
            transcript(ToolStatus::Running, None).settle(Some(&new_anchor)),
            TurnSettle::Complete,
            "the carried panel is display-only: the Turn's settle decision is unchanged"
        );

        // The transcript finally records the failed run: the panel reconciles.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Error, Some("killed"))
        ));
        assert!(!acc.tools["call_sleep"].is_live());
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("killed").count(), 1, "reconciled once: {text}");
    }

    /// The canonical restart window (ADR-0068): the fresh Turn's message is
    /// queued behind the still-running orphan run, so the transcript carries no
    /// anchor for as long as that run lasts. A carried call is resolved by call
    /// identity against the whole read, so its completion reconciles into the
    /// timeline anyway — while the anchor is unobserved — exactly once; the
    /// later anchor capture neither duplicates nor re-orders it.
    #[test]
    fn a_carried_call_reconciles_before_the_turn_anchor_is_observed() {
        let anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: anchor - 31 * 60_000,
        };
        let stale_tool = |status: ToolStatus, output: Option<&str>| {
            vec![tool(
                "shell",
                "call_sleep",
                status,
                Some(anchor - 11 * 60_000),
                Some(serde_json::json!({"command": "sleep 3600"})),
                output,
            )]
        };
        // The orphan's own message only: the fresh Turn's user message is NOT
        // in the transcript yet (queued behind the orphan run), so
        // `capture_turn_anchor` cannot find an anchor.
        let anchorless = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![message_in_flight(
                "a_stale",
                anchor - 30 * 60_000,
                None,
                stale_tool(status, output),
            )])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_new".into());
        assert_eq!(
            acc.carry_tools(&anchorless(ToolStatus::Running, None).turn_running_tools(&orphan)),
            1
        );
        assert_eq!(acc.turn_anchor, None);

        // The call completes while the fresh message is still absent: the
        // carried identity reconciles anyway, joining the timeline at its
        // server start key.
        assert!(render_new_turn_parts(
            &mut acc,
            &anchorless(ToolStatus::Completed, Some("slept"))
        ));
        assert_eq!(acc.turn_anchor, None, "still no anchor to gate on");
        assert!(!acc.tools["call_sleep"].is_live());
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("slept").count(), 1, "reconciled once: {text}");

        // Repeated anchorless reads must not duplicate it.
        assert!(!render_new_turn_parts(
            &mut acc,
            &anchorless(ToolStatus::Completed, Some("slept"))
        ));
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("slept").count(), 1, "no duplicate: {text}");

        // The fresh message finally lands (the orphan run released the queue):
        // the anchor is captured, the window renders, and the settled call
        // neither duplicates nor loses its server start order.
        let with_anchor = SessionTranscript::new(vec![
            message_in_flight(
                "a_stale",
                anchor - 30 * 60_000,
                None,
                stale_tool(ToolStatus::Completed, Some("slept")),
            ),
            typed_message(
                "msg_cola_new",
                MessageRole::User,
                Some(anchor),
                vec![text_part("新的问题")],
            ),
            message("a_new", anchor + 1_000, vec![text_part("新回答")]),
        ]);
        assert!(render_new_turn_parts(&mut acc, &with_anchor));
        assert_eq!(
            acc.turn_anchor,
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_new"),
                created_ms: anchor,
            })
        );
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("slept").count(), 1, "still settled once: {text}");
        assert!(
            text.find("slept") < text.find("新回答"),
            "the settled call keeps its server start order: {text}"
        );
    }

    /// A carried call the Turn's own window renders is the Turn's own live
    /// panel again: it leaves the display-only carry set, so the ordinary
    /// live-panel rules (#284's guard included) apply exactly as before the
    /// carry existed (ADR-0068's in-window regression).
    #[test]
    fn an_in_window_render_readopts_a_carried_call() {
        let anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: anchor - 60_000,
        };
        let transcript = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_new",
                    MessageRole::User,
                    Some(anchor),
                    vec![text_part("新的问题")],
                ),
                message_in_flight(
                    "a_inflight",
                    anchor - 60_000,
                    None,
                    vec![tool(
                        "shell",
                        "call_sleep",
                        status,
                        Some(anchor - 30_000),
                        Some(serde_json::json!({"command": "sleep 600"})),
                        output,
                    )],
                ),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_new".into());
        assert_eq!(
            acc.carry_tools(&transcript(ToolStatus::Running, None).turn_running_tools(&orphan)),
            1
        );
        assert!(acc.carried_calls.contains("call_sleep"));

        render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
        assert!(
            acc.carried_calls.is_empty(),
            "the Turn's own window render owns the call now, not the carry"
        );
    }

    /// The successor's own reads rebuild the todo list, so a running
    /// `todowrite` is never carried (ADR-0068's carry set: the todo list, the
    /// ledger and the interaction blocks stay out).
    #[test]
    fn a_running_todowrite_is_never_carried() {
        let mut acc = StreamAccumulator::new("proj");
        let call = ToolCall {
            identity: ToolIdentity {
                name: "todowrite".into(),
                call_id: "call_todo".into(),
            },
            status: ToolStatus::Running,
            started_at: Some(1_000),
            input: Some(serde_json::json!({"todos": []})),
            metadata: None,
            output: ToolOutput::default(),
        };

        assert_eq!(
            acc.carry_tools(&[call]),
            0,
            "the successor's own reads rebuild the todo list"
        );
        assert!(acc.tools.is_empty() && acc.carried_calls.is_empty());
    }

    /// A killed run's carried call does not outlive the Turn (ADR-0068): while
    /// a live renderer owns the card the panel rides the tail and reconciles,
    /// but once the card settles no renderer will ever update it again, so a
    /// still-running carried panel is not built — no permanent `⏳` on the
    /// final card. A carried call that settled before the end is a timeline
    /// entry and still renders exactly once.
    #[test]
    fn a_still_running_carried_call_is_not_built_once_the_card_is_settled() {
        let anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: anchor - 31 * 60_000,
        };
        let transcript = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_new",
                    MessageRole::User,
                    Some(anchor),
                    vec![text_part("新的问题")],
                ),
                message_in_flight(
                    "a_stale",
                    anchor - 30 * 60_000,
                    None,
                    vec![tool(
                        "shell",
                        "call_sleep",
                        status,
                        Some(anchor - 11 * 60_000),
                        Some(serde_json::json!({"command": "sleep 3600"})),
                        output,
                    )],
                ),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_new".into());
        acc.carry_tools(&transcript(ToolStatus::Running, None).turn_running_tools(&orphan));
        render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
        assert!(
            card_text(&acc.build_card_with_split().0).contains("⏳ shell"),
            "a live renderer owns the card, so the carried panel rides the tail"
        );

        // The Turn ends with the call still running (the killed-run case): the
        // settled card omits the panel — the `⏳` never outlives the Turn.
        acc.card_state = CardState::Done;
        let text = card_text(&acc.build_card_with_split().0);
        assert!(
            !text.contains("⏳ shell"),
            "the settled card must not keep a permanent running panel: {text}"
        );
        // A yielded card has no renderer either: same rule.
        acc.card_state = CardState::Waiting;
        assert!(
            !card_text(&acc.build_card_with_split().0).contains("⏳ shell"),
            "a card no renderer owns never shows a still-running carried panel"
        );

        // A completion that landed before the end is a timeline record and
        // still renders on the settled card, exactly once.
        acc.card_state = CardState::Streaming;
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, Some("slept"))
        ));
        acc.card_state = CardState::Done;
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(
            text.matches("slept").count(),
            1,
            "a settled carried call still renders on the final card: {text}"
        );
    }

    /// The header date reads the SERVER's time for the turn's user message
    /// (#183 follow-up): captured on the first poll that sees it, so cola's
    /// own clock never reaches the card. The same captured anchor is #190's
    /// turn filter, so this test also pins the capture source.
    #[test]
    fn turn_anchor_captures_the_user_message_identity_and_server_time() {
        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let transcript = SessionTranscript::new(vec![typed_message(
            "msg_cola_1",
            MessageRole::User,
            Some(1234),
            vec![text_part("你好")],
        )]);

        assert!(!render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(
            acc.turn_anchor,
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1234,
            })
        );
    }

    /// Until the server's own user message is observed there is no anchor, and
    /// nothing renders: cola's clock is no substitute — with skewed clocks any
    /// threshold it provides either drops the new turn or admits the old one.
    #[test]
    fn nothing_renders_before_the_server_anchor_is_observed() {
        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let transcript = SessionTranscript::new(vec![message("a1", 100, vec![text_part("回答")])]);

        assert!(!render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(acc.turn_anchor, None);
        assert!(acc.text.is_empty());
    }

    /// #190: a server clock BEHIND cola's must not drop the new turn — every
    /// part is "in the past" for cola, so the retired cola-clock filter
    /// skipped them and the card stayed empty. The filter anchors on the
    /// server's own user-message time instead.
    #[test]
    fn a_server_clock_behind_cola_does_not_drop_the_turns_parts() {
        let cola_now = chrono::Utc::now().timestamp_millis();
        let server_user = cola_now - 3_600_000; // the server is an hour behind
        let assistant = server_user + 250;

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_1",
                MessageRole::User,
                Some(server_user),
                vec![text_part("你好")],
            ),
            message("a1", assistant, vec![text_part("回答")]),
        ]);
        assert!(
            assistant < cola_now,
            "fixture: the whole turn predates cola's clock"
        );

        assert!(render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(
            acc.turn_anchor,
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: server_user,
            })
        );
        assert!(
            acc.text.contains("回答"),
            "the turn's parts must render: {:?}",
            acc.text
        );
    }

    /// #190: a server clock AHEAD of cola's must not bleed the previous turn
    /// into the new card. The previous assistant message is after cola's
    /// submit clock but before this turn's user message, so only the server
    /// anchor separates the two turns.
    #[test]
    fn a_server_clock_ahead_of_cola_does_not_bleed_the_previous_turn() {
        let cola_now = chrono::Utc::now().timestamp_millis();
        let previous_assistant = cola_now + 30_000; // still future to cola
        let server_user = cola_now + 60_000; // this turn's user message
        let assistant = server_user + 250;

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let transcript = SessionTranscript::new(vec![
            message("a_old", previous_assistant, vec![text_part("旧回答")]),
            typed_message(
                "msg_cola_1",
                MessageRole::User,
                Some(server_user),
                vec![text_part("你好")],
            ),
            message("a1", assistant, vec![text_part("新回答")]),
        ]);
        assert!(
            previous_assistant > cola_now,
            "fixture: the previous turn is still ahead of cola's clock"
        );

        assert!(render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(
            acc.turn_anchor,
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: server_user,
            })
        );
        assert!(
            acc.text.contains("新回答"),
            "the turn must render: {:?}",
            acc.text
        );
        assert!(
            !acc.text.contains("旧回答"),
            "the previous turn must not bleed in: {:?}",
            acc.text
        );
    }

    /// #183 follow-up: only parts with a server time show one. A part whose
    /// payload carries no `time` (a pending tool) is placed by a fallback key
    /// and shows no clock — rendering cola's moment there would put cola's
    /// clock in the same costume as the server's. When the running state later
    /// arrives with the server's start time, the panel gains it without moving.
    #[test]
    fn part_without_server_time_shows_no_clock_until_the_server_stamps_it() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 17, 0, 5);
        let mut acc = StreamAccumulator::new("proj");
        render_parts(
            &mut acc,
            &[
                reasoning_at("thinking", at),
                tool("bash", "call_1", ToolStatus::Pending, None, None, None),
            ],
        );
        let card = acc.build_card().to_string();
        assert!(card.contains("💭 推理过程 · 00:05"), "{card}");
        assert!(
            card.contains("⏳ bash") && !card.contains("⏳ bash ·"),
            "a pending tool must show no clock: {card}"
        );

        // The server stamps the call as it starts running: the same panel
        // shows the start time from now on.
        render_parts(
            &mut acc,
            &[tool(
                "bash",
                "call_1",
                ToolStatus::Running,
                Some(at),
                Some(serde_json::json!({"command": "sleep 2"})),
                None,
            )],
        );
        let card = acc.build_card().to_string();
        assert!(card.contains("⏳ bash · 00:05"), "{card}");
    }

    #[test]
    fn tool_part_update_re_renders_panel() {
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        let transcript = |status: ToolStatus, output: &str| {
            SessionTranscript::new(vec![message(
                "a1",
                100,
                vec![tool(
                    "bash",
                    "call_1",
                    status,
                    None,
                    Some(serde_json::json!({"command": "ls"})),
                    Some(output),
                )],
            )])
        };

        // First render: tool running.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Running, "")
        ));
        assert_eq!(acc.tools["call_1"].status(), &ToolStatus::Running);

        // Same call, updated to completed — must re-render (upsert).
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, "src\n")
        ));
        assert_eq!(acc.tools["call_1"].status(), &ToolStatus::Completed);

        // No change → nothing new.
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, "src\n")
        ));
    }

    /// The model re-sends the whole todo list on every update, each as a NEW
    /// `todowrite` call (a new correlation id). The panel is a card-TAIL status
    /// section:
    /// no timeline row, the latest call replaces it in place, and its header
    /// carries the latest list's counts (visible while folded) and clock.
    #[test]
    fn todowrite_calls_share_one_panel_refreshed_in_place() {
        let at_a = crate::feishu::card::test_local_ms(2026, 9, 17, 10, 0);
        let at_b = crate::feishu::card::test_local_ms(2026, 9, 17, 10, 7);
        let mut acc = StreamAccumulator::new("test");
        render_parts(
            &mut acc,
            &[
                todowrite(
                    "call_a",
                    at_a,
                    serde_json::json!([
                        { "content": "第一步", "status": "in_progress", "priority": "high" }
                    ]),
                ),
                todowrite(
                    "call_b",
                    at_b,
                    serde_json::json!([
                        { "content": "第一步", "status": "completed", "priority": "high" },
                        { "content": "第二步", "status": "pending", "priority": "medium" }
                    ]),
                ),
            ],
        );

        assert!(
            acc.tools.is_empty(),
            "todowrite must not take a timeline row: {:?}",
            acc.tools.keys().collect::<Vec<_>>()
        );
        let card = acc.build_card().to_string();
        assert_eq!(
            card.matches("todowrite").count(),
            1,
            "exactly one todo panel must render: {card}"
        );
        // The latest call's list, not the first one's.
        assert!(card.contains("- ✅ ~~第一步~~"), "latest status: {card}");
        assert!(card.contains("- ⬜ 第二步"), "latest item: {card}");
        assert!(
            !card.contains("**第一步**"),
            "the first call's in-progress row must be gone: {card}"
        );
        // The header shows the latest update's clock, size and counts.
        assert!(
            card.contains("📋 todowrite · 10:07 · 共 2 项"),
            "latest clock and size: {card}"
        );
        assert!(card.contains("· ⬜ 1 · ✅ 1"), "header counts: {card}");
        assert!(!card.contains("10:00"), "the first clock must be gone: {card}");
    }

    /// The tail is what makes the live list survive a split: a timeline row
    /// would freeze on whichever card the call landed on, and later updates
    /// (which re-render the live continuation) could never reach it.
    #[test]
    fn todowrite_panel_rides_the_live_continuation_after_a_split() {
        let mut acc = StreamAccumulator::new("test");
        render_parts(
            &mut acc,
            &[todowrite(
                "call_a",
                0,
                serde_json::json!([
                    { "content": "第一步", "status": "in_progress" }
                ]),
            )],
        );
        // Enough text to push the timeline past one card's budget.
        acc.push_text(&"很长的回答。".repeat(2000));

        let (full_card, full) = acc.build_card_with_split();
        assert!(full, "long text must split");
        assert!(
            !full_card.to_string().contains("todowrite"),
            "a finalized card carries no tail: {}",
            full_card
        );

        // The continuation is the live card; the todo panel rides it, so a
        // later list update is visible there.
        render_parts(
            &mut acc,
            &[todowrite(
                "call_b",
                0,
                serde_json::json!([
                    { "content": "第一步", "status": "completed" },
                    { "content": "第二步", "status": "pending" }
                ]),
            )],
        );
        let (rest, full2) = acc.build_card_with_split();
        assert!(!full2, "the tail should fit on the continuation");
        let rest_text = rest.to_string();
        assert!(
            rest_text.contains("- ✅ ~~第一步~~") && rest_text.contains("- ⬜ 第二步"),
            "the continuation must show the latest list: {rest_text}"
        );
    }

    /// The todo tail's reserve must never wedge the card chain: when the first
    /// remaining item plus the panel's reserve exceeds the budget, that item is
    /// finalized WITHOUT the tail (never an empty slice, which would freeze
    /// `render_from` and re-send blank "部分完成" cards forever) and the panel
    /// rides a later card.
    #[test]
    fn todo_reserve_never_wedges_the_card_chain() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        // A near-maximal todo panel: one item whose CJK content fills the
        // reserve's 3000-char output window (~9KB).
        let output = serde_json::json!([{
            "content": "很长的任务描述".repeat(500),
            "status": "pending",
        }])
        .to_string();
        acc.todo_panel = Some(crate::feishu::card::tool_render::ToolPanel::for_test(
            "todowrite",
            ToolStatus::Completed,
            None,
            Some(&output),
        ));
        // Two max-size CJK text chunks (~18KB each): the first alone plus the
        // reserve is over the budget, so only the progress guarantee keeps the
        // chain moving.
        acc.push_text(&"很长的回答。".repeat(1000));
        acc.push_text(&"另一段回答。".repeat(1000));

        let mut cards = 0;
        loop {
            let before = acc.render_from;
            let (card, full) = acc.build_card_with_split();
            let text = card.to_string();
            assert!(
                text.contains("回答。") || text.contains("todowrite"),
                "an empty card was built at render_from={before}: {text}"
            );
            cards += 1;
            assert!(cards < 10, "the card chain did not terminate");
            if !full {
                break;
            }
            assert!(acc.render_from > before, "a full card must advance render_from");
        }
        let last = acc.build_card();
        assert!(
            last.to_string().contains("todowrite"),
            "the panel must ride the final card: {last}"
        );
    }

    /// A todowrite update whose content has the same byte length as the
    /// previous one (任务 A → 任务 B) must still refresh the panel: length alone
    /// is not a change signal for a re-written list. The same-call case is the
    /// in-place update the accumulator must never skip — the mutation audit
    /// (render.rs:372) found the todowrite branch surviving a different-call
    /// test alone.
    #[test]
    fn todowrite_same_length_update_still_refreshes() {
        let mut acc = StreamAccumulator::new("test");
        render_parts(
            &mut acc,
            &[todowrite(
                "call_a",
                0,
                serde_json::json!([{ "content": "任务 A", "status": "pending" }]),
            )],
        );
        render_parts(
            &mut acc,
            &[todowrite(
                "call_b",
                0,
                serde_json::json!([{ "content": "任务 B", "status": "pending" }]),
            )],
        );

        let card = acc.build_card().to_string();
        assert!(card.contains("任务 B"), "latest content must render: {card}");
        assert!(
            !card.contains("任务 A"),
            "the stale list must be replaced: {card}"
        );
        assert!(acc.todo_panel.is_some(), "the tail holds the panel");

        // The SAME call re-streams with new, equal-length content: the panel
        // must follow it, not dedupe on a length-only signature.
        let mut acc = StreamAccumulator::new("test");
        render_parts(
            &mut acc,
            &[todowrite(
                "call_same",
                0,
                serde_json::json!([{ "content": "任务 C", "status": "pending" }]),
            )],
        );
        render_parts(
            &mut acc,
            &[todowrite(
                "call_same",
                0,
                serde_json::json!([{ "content": "任务 D", "status": "pending" }]),
            )],
        );
        let card = acc.build_card().to_string();
        assert!(
            card.contains("任务 D"),
            "an in-place equal-length update must refresh: {card}"
        );
        assert!(
            !card.contains("任务 C"),
            "the stale in-place list must be replaced: {card}"
        );
    }

    /// Every collapsible panel carries a stable `element_id`, derived from the
    /// timeline entry it renders — never from its position. The card is
    /// re-rendered whole on every flush while the client holds each panel's
    /// open/closed state locally; an id that moved would hand that state to a
    /// different panel, which is how the todo panel used to lose its expansion.
    #[test]
    fn panel_element_ids_stay_with_their_timeline_item() {
        let panel_ids = |card: &serde_json::Value| -> Vec<String> {
            card["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["tag"] == "collapsible_panel")
                .map(|e| {
                    format!(
                        "{}({})",
                        e["element_id"].as_str().expect("panel element_id"),
                        e["header"]["title"]["content"].as_str().unwrap()
                    )
                })
                .collect()
        };

        let mut acc = StreamAccumulator::new("test");
        render_parts(
            &mut acc,
            &[
                reasoning_part("想一想"),
                tool("bash", "call_1", ToolStatus::Completed, None, None, Some("ok")),
                todowrite(
                    "call_todo",
                    0,
                    serde_json::json!([{ "content": "第一步", "status": "pending" }]),
                ),
            ],
        );
        let before = panel_ids(&acc.build_card());
        assert_eq!(before.len(), 3, "reasoning, tool, todo: {before:?}");
        assert!(before[0].starts_with("reason_"), "{before:?}");
        assert!(before[1].starts_with("tool_"), "{before:?}");
        assert!(
            before[2].starts_with("todo("),
            "the tail panel is the todo: {before:?}"
        );

        // A part that arrives late but sorts first must not steal the ids of
        // the entries it was inserted before.
        acc.push_reasoning_at(Some(0), "迟到的推理");
        let after = panel_ids(&acc.build_card());
        assert_eq!(after.len(), 4, "{after:?}");
        assert!(
            after[0].starts_with("reason_") && !before.contains(&after[0]),
            "the late part gets its own fresh id: {after:?}"
        );
        assert_eq!(&after[1..], &before[..], "existing panels keep their ids");
    }

    #[test]
    fn empty_then_updated_part_renders_once_with_content() {
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        let transcript = |reasoning: &str, text: &str| {
            SessionTranscript::new(vec![message(
                "a1",
                100,
                vec![reasoning_part(reasoning), text_part(text)],
            )])
        };

        // Parts are written empty first, then updated with content. The empty
        // version must NOT be rendered (it would freeze the placeholder).
        assert!(!render_new_turn_parts(&mut acc, &transcript("", "")));
        assert_eq!(acc.reasoning, "");
        assert_eq!(acc.text, "");

        // Once content lands (same message/parts), render it once.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript("Let me think", "Answer here")
        ));
        assert!(acc.reasoning.contains("Let me think"));
        assert!(acc.text.contains("Answer here"));

        // Re-fetching the same content must not duplicate.
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript("Let me think", "Answer here")
        ));
        assert_eq!(acc.reasoning, "Let me think");
        assert_eq!(acc.text, "Answer here");
    }

    /// Regression: real OpenCode part payloads carry NO `id` field (the DB id
    /// is not serialised into the part JSON). Dedup must fall back to content,
    /// otherwise the poll loop + final render append the same text twice
    /// (observed: card text 81 → 162 chars).
    #[test]
    fn text_without_id_is_not_rendered_twice() {
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        // Realistic: no part id, just typed text and reasoning.
        let transcript = || {
            SessionTranscript::new(vec![message(
                "a1",
                100,
                vec![text_part("你好！很高兴认识你。"), reasoning_part("thinking")],
            )])
        };

        // Poll loop renders the parts.
        assert!(render_new_turn_parts(&mut acc, &transcript()));
        assert_eq!(acc.text, "你好！很高兴认识你。");

        // Final render re-fetches the same messages — must NOT append again.
        assert!(!render_new_turn_parts(&mut acc, &transcript()));
        assert_eq!(acc.text, "你好！很高兴认识你。");
        assert_eq!(acc.reasoning, "thinking");
    }

    /// Real OpenCode failed-tool calls use status `error` with the reason in
    /// the output's error side and no output text — the panel must show the
    /// error and mark the card failed, not stay stuck "running".
    #[test]
    fn failed_tool_error_part_renders_output_and_error_state() {
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        let mut call = tool_call(
            "edit",
            "call_edit",
            ToolStatus::Error,
            None,
            Some(serde_json::json!({"filePath": "src/main.rs", "oldString": "a", "newString": "b"})),
            vec![crate::backend::ContentBlock::Text("something went wrong".into())],
        );
        call.output.error = Some("no such file".into());
        let transcript = SessionTranscript::new(vec![message("a1", 100, vec![Part::Tool(call)])]);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        let tool = acc.tools.get("call_edit").expect("tool rendered");
        assert_eq!(tool.status(), &ToolStatus::Error);
        let out = tool.output().unwrap_or_default();
        assert!(
            out.contains("no such file"),
            "error message must be shown: {}",
            out
        );

        // The Done card header stays green — the failure is on the tool's own
        // panel, not the whole turn.
        acc.card_state = crate::feishu::card::CardState::Done;
        let card = acc.build_card();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(
            header.contains("完成"),
            "failed tool alone must not fail the card: {}",
            header
        );
    }

    /// Some tools (e.g. `edit` with a stale `oldString`) put the reason in a
    /// PLAIN STRING error (the decoder normalizes it to the output's error
    /// side) — it must still show up on the panel, not vanish.
    #[test]
    fn failed_tool_string_error_renders_on_panel() {
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        let mut call = tool_call(
            "edit",
            "call_edit",
            ToolStatus::Error,
            None,
            Some(serde_json::json!({"filePath": "src/main.rs", "oldString": "a", "newString": "b"})),
            Vec::new(),
        );
        call.output.error = Some("Could not find oldString in the file. It must match exactly.".into());
        let transcript = SessionTranscript::new(vec![message("a1", 100, vec![Part::Tool(call)])]);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        let tool = acc.tools.get("call_edit").expect("tool rendered");
        assert_eq!(tool.status(), &ToolStatus::Error);
        let out = tool.output().unwrap_or_default();
        assert!(
            out.contains("Could not find oldString"),
            "string error message must be shown: {:?}",
            out
        );
        assert!(!out.is_empty(), "output must not be empty for a string error");
    }
    /// A completed `edit` records its real unified diff in its raw metadata
    /// (the text output only says "Edit applied successfully."). The panel
    /// output must carry the diff so the card shows what actually changed
    /// instead of the file name + the tool's success sentence. Failures keep
    /// their extracted error text.
    #[test]
    fn edit_part_uses_metadata_diff_as_output() {
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        let diff = "Index: src/main.rs\n======\n--- src/main.rs\n+++ src/main.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let edit = |status: ToolStatus, output: Option<&str>, with_diff: bool, error: Option<&str>| {
            let mut call = tool_call(
                "edit",
                "call_edit",
                status,
                None,
                Some(serde_json::json!({"filePath": "src/main.rs", "oldString": "a", "newString": "b"})),
                output
                    .map(|text| vec![crate::backend::ContentBlock::Text(text.to_string())])
                    .unwrap_or_default(),
            );
            call.output.error = error.map(str::to_string);
            if with_diff {
                call.metadata = Some(serde_json::json!({
                    "diagnostics": {},
                    "diff": diff,
                    "filediff": { "file": "src/main.rs", "additions": 1, "deletions": 1 }
                }));
            }
            Part::Tool(call)
        };
        let transcript = |part: Part| SessionTranscript::new(vec![message("a1", 100, vec![part])]);

        // Completed: the diff replaces the generic success sentence.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(edit(
                ToolStatus::Completed,
                Some("Edit applied successfully."),
                true,
                None
            ))
        ));
        let out = acc.tools["call_edit"].output().unwrap_or_default();
        assert!(out.contains("@@ -1 +1 @@"), "diff must be shown: {}", out);
        assert!(
            !out.contains("Edit applied successfully."),
            "noise dropped: {}",
            out
        );

        // Re-rendering the same completed part is deduped (output unchanged).
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript(edit(
                ToolStatus::Completed,
                Some("Edit applied successfully."),
                true,
                None
            ))
        ));

        // A failure keeps its error text, not a diff.
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(edit(ToolStatus::Error, None, true, Some("no such file")))
        ));
        let out = acc.tools["call_edit"].output().unwrap_or_default();
        assert!(out.contains("no such file"), "error text must be shown: {}", out);
        assert!(!out.contains("@@"), "no diff on a failed edit: {}", out);
    }

    /// Regression: the final reconcile falls back to `render_parts(resp.parts)`
    /// when the incremental poll already rendered everything (`render_new_turn_parts`
    /// returns false). `render_parts` used to append unconditionally, doubling the
    /// card text on long turns (the poll renders the answer while the card is still
    /// "streaming", then the final card repeats it).
    #[test]
    fn render_parts_fallback_does_not_double_already_rendered_text() {
        let mut acc = StreamAccumulator::new("test");
        acc.turn_anchor = Some(turn_anchor(0));

        let parts = vec![
            reasoning_part("Let me check"),
            text_part("The answer."),
            tool(
                "bash",
                "call_1",
                ToolStatus::Completed,
                None,
                Some(serde_json::json!({"command": "ls"})),
                Some("src"),
            ),
        ];
        let transcript = SessionTranscript::new(vec![message("a1", 100, parts.clone())]);

        // Long turn: the poll loop already rendered the parts.
        assert!(render_new_turn_parts(&mut acc, &transcript));

        // Final reconcile: nothing new from the transcript → falls back to the
        // response parts (identical content). Must NOT append again.
        assert!(!render_parts(&mut acc, &parts));
        assert_eq!(acc.text, "The answer.");
        assert_eq!(acc.reasoning, "Let me check");
        assert_eq!(acc.tools["call_1"].output().as_deref(), Some("src"));
    }

    /// A header change alone must re-flush the card: the progress timer keeps
    /// ticking, so an idle turn still proves it is alive (ADR-0014). The
    /// mutation audit (render.rs:508) found the inverted header comparison
    /// surviving the suite.
    #[tokio::test]
    async fn render_and_flush_flushes_on_a_header_change_alone() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let sid = "ses_header";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_header")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        // No parts at all: only the header signature can trigger a flush.
        let transcript = SessionTranscript::default();
        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &transcript,
        )
        .await;
        assert_eq!(
            platform.updated_cards().await.len(),
            1,
            "the seeded header must flush once"
        );

        // The header signature moves (the state label flips) with no new parts.
        Turn::set_card_state(&cards, sid, CardState::Reasoning).await;
        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &transcript,
        )
        .await;
        let updates = platform.updated_cards().await;
        assert_eq!(
            updates.len(),
            2,
            "a header change alone must re-flush the card: {updates:?}"
        );
        assert!(
            updates.last().unwrap().to_string().contains("推理中"),
            "the re-flush carries the new header: {}",
            updates.last().unwrap()
        );
    }

    /// New content must flush immediately, without waiting for the header's
    /// whole-second tick: the mutation audit (render.rs:540) found the content
    /// disjunct surviving because most tests' content changes also move the
    /// header signature.
    #[tokio::test]
    async fn render_and_flush_flushes_on_new_parts_without_a_header_tick() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let sid = "ses_content";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_content")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let transcript =
            |text: &str| SessionTranscript::new(vec![message("a1", 1_000, vec![text_part(text)])]);

        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &transcript("第一段"),
        )
        .await;
        assert_eq!(platform.updated_cards().await.len(), 1);

        // Same second, same state, no usage: only the new text differs.
        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &transcript("第二段"),
        )
        .await;
        let updates = platform.updated_cards().await;
        assert_eq!(
            updates.len(),
            2,
            "new content must flush without waiting for the header tick: {updates:?}"
        );
        assert!(
            updates.last().unwrap().to_string().contains("第二段"),
            "the flushed card carries the new text: {}",
            updates.last().unwrap()
        );
    }

    /// Criterion 3 (spec #501, ticket #504): on the LIVE render path one typed
    /// liveness change owes exactly one card PATCH — the seeded card's header
    /// is frozen, so the count is the ledger's own — while the activity age's
    /// seconds inside a rendered minute owe none. The fragment's clock sits
    /// half a minute back, far from a minute boundary, so the window cannot
    /// cross one.
    #[tokio::test]
    async fn render_and_flush_flushes_once_on_a_typed_liveness_change_and_never_on_age_seconds() {
        let _wd = test_work_dir();
        let now = chrono::Utc::now().timestamp_millis();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        backend
            .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 30_000, "bash")])
            .await;
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
        let sid = "ses_ledger_flush";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_ledger_flush")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;
        // The phase timer is a live card's per-second header tick; freezing it
        // keeps every PATCH counted below the ledger's own.
        Turn::clear_phase(&cards, sid).await;

        let transcript = SessionTranscript::default().with_background_tasks(vec![BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_sub".into(),
            },
            shell_id: None,
            child_id: Some("ses_call_sub".into()),
            started_at: Some(now - 30_000),
        }]);
        let render = async || {
            let _ = render_and_flush(
                &cards,
                &app.sessions_handle(),
                &app.opencode,
                &app.requests_handle(),
                sid,
                &transcript,
            )
            .await;
        };

        // The row arrives with its child's fragment: one PATCH.
        render().await;
        assert_eq!(
            platform.updated_cards().await.len(),
            1,
            "the row's arrival flushes once"
        );
        // The same read again: the fragment's age ticked a second inside its
        // rendered minute — the ledger owes nothing, and nothing else moved.
        render().await;
        assert_eq!(
            platform.updated_cards().await.len(),
            1,
            "same-minute age seconds owe no PATCH"
        );

        // The child switches tools: one typed change, exactly one PATCH.
        backend
            .given_transcript_after_build("ses_call_sub", vec![child_running_tool(now - 30_000, "edit")])
            .await;
        render().await;
        let updates = platform.updated_cards().await;
        assert_eq!(
            updates.len(),
            2,
            "one typed liveness change owes exactly one PATCH: {updates:?}"
        );
        assert!(
            card_text(updates.last().unwrap()).contains("· edit "),
            "the PATCH carries the new fragment: {}",
            updates.last().unwrap()
        );

        // The new fragment's seconds tick on inside its minute: no PATCH.
        render().await;
        assert_eq!(
            platform.updated_cards().await.len(),
            2,
            "the typed change's own age seconds owe none"
        );
    }

    /// A child-session transcript whose newest part is a running tool — the
    /// activity a ledger row renders as `<name> <age>`.
    fn child_running_tool(started_at: i64, name: &str) -> SessionTranscript {
        SessionTranscript::new(vec![message(
            "a_child",
            started_at,
            vec![tool(
                name,
                "call_child",
                ToolStatus::Running,
                Some(started_at),
                None,
                None,
            )],
        )])
    }

    /// Spec #501: the render's child gather is SHARED by the ledger's rows and
    /// the live task panels, so a child named by both surfaces is read — and
    /// its wait queried — at most once per render. The card carries a live
    /// `task` panel whose child is the same `ses_shared` its ledger row names;
    /// one render must spend one read and one wait for it, and both surfaces
    /// must render the fragment.
    #[tokio::test]
    async fn one_render_reads_a_child_shared_by_a_panel_and_a_ledger_row_once() {
        let _wd = test_work_dir();
        let now = chrono::Utc::now().timestamp_millis();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        backend
            .given_transcript_after_build("ses_shared", vec![child_running_tool(now - 20_000, "bash")])
            .await;
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
        let sid = "ses_shared_gather";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_shared_gather")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;
        Turn::clear_phase(&cards, sid).await;

        // A live `task` panel on the card names the same child as the ledger.
        {
            let mut live = cards.cards.lock().await;
            let acc = &mut live.get_mut(sid).expect("the seeded card").acc;
            acc.push_tool_at(
                Some(now - 20_000),
                "call_panel",
                ToolPanel::new(ToolCall {
                    identity: ToolIdentity {
                        name: "task".into(),
                        call_id: "call_panel".into(),
                    },
                    status: ToolStatus::Running,
                    started_at: Some(now - 20_000),
                    input: Some(serde_json::json!({ "description": "review the shared child" })),
                    metadata: Some(serde_json::json!({ "sessionID": "ses_shared" })),
                    output: ToolOutput::default(),
                }),
            );
        }
        let transcript = SessionTranscript::default().with_background_tasks(vec![BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_sub".into(),
            },
            shell_id: None,
            child_id: Some("ses_shared".into()),
            started_at: Some(now - 30_000),
        }]);

        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &transcript,
        )
        .await;

        // One transcript read and one wait query for the shared child, however
        // many surfaces name it (`wait_for` checks both flows per query).
        let reads = backend
            .transcript_calls
            .lock()
            .await
            .iter()
            .filter(|id| id.as_str() == "ses_shared")
            .count();
        assert_eq!(reads, 1, "the shared child is read once per render");
        let requests = app.requests_handle();
        for flow in [&requests.permission, &requests.question] {
            let checks = flow
                .wait_checks
                .lock()
                .await
                .iter()
                .filter(|id| id.as_str() == "ses_shared")
                .count();
            assert_eq!(checks, 1, "the shared child's wait is queried once per render");
        }

        // Both surfaces render the fragment from that one gather, on one PATCH.
        assert_eq!(platform.updated_cards().await.len(), 1, "one render, one PATCH");
        let card = platform.updated_cards().await.last().cloned().unwrap();
        let text = card_text(&card);
        assert!(
            text.lines()
                .any(|line| line.starts_with("· subagent") && line.contains("bash ")),
            "the ledger row carries the fragment: {text}"
        );
        assert!(
            text.lines()
                .any(|line| line.contains("task") && line.contains("bash ")),
            "the live panel carries the same fragment: {text}"
        );
    }

    /// ADR-0054: the child session's liveness is the age of its newest activity
    /// (message completion or part start) and its newest still-running tool.
    #[test]
    fn child_liveness_reads_age_and_current_tool() {
        let transcript = SessionTranscript::new(vec![
            typed_message("u", MessageRole::User, Some(1_000), vec![]),
            message(
                "a1",
                5_000,
                vec![
                    reasoning_at("thinking", 4_000),
                    tool("read", "c1", ToolStatus::Completed, Some(6_000), None, None),
                    tool("bash", "c2", ToolStatus::Running, Some(9_000), None, None),
                ],
            ),
        ]);
        assert_eq!(
            child_liveness(&transcript),
            Some(TaskLiveness {
                activity: ChildActivity::Tool {
                    name: "bash".into(),
                    started_at: Some(9_000),
                },
                last_activity_ms: 9_000,
                wait: None,
            })
        );
    }

    /// A child with no timestamp at all renders no line — a made-up age would
    /// be a lie, and an empty transcript is normal for a just-started child.
    #[test]
    fn child_liveness_without_any_timestamp_is_none() {
        let transcript = SessionTranscript::new(vec![typed_message("u", MessageRole::User, None, vec![])]);
        assert_eq!(child_liveness(&transcript), None);
    }

    /// While the child thinks (no live tool), only the age is shown.
    #[test]
    fn child_liveness_reports_no_tool_while_thinking() {
        let transcript = SessionTranscript::new(vec![message(
            "a1",
            5_000,
            vec![
                tool("bash", "c1", ToolStatus::Completed, Some(6_000), None, None),
                reasoning_at("thinking", 8_000),
            ],
        )]);
        assert_eq!(
            child_liveness(&transcript),
            Some(TaskLiveness {
                activity: ChildActivity::Reasoning,
                last_activity_ms: 8_000,
                wait: None,
            })
        );
    }

    /// The phase follows the newest part: streamed reply text reads 回复中, and
    /// a completed tool with nothing newer reads 思考中 (the model is working
    /// through the result).
    #[test]
    fn child_liveness_derives_the_phase_from_the_newest_part() {
        let replying = SessionTranscript::new(vec![message("a1", 5_000, vec![text_part("hi")])]);
        assert_eq!(
            child_liveness(&replying),
            Some(TaskLiveness {
                activity: ChildActivity::Replying,
                last_activity_ms: 5_000,
                wait: None,
            })
        );

        let thinking = SessionTranscript::new(vec![message(
            "a1",
            5_000,
            vec![tool("bash", "c1", ToolStatus::Completed, Some(6_000), None, None)],
        )]);
        assert_eq!(
            child_liveness(&thinking),
            Some(TaskLiveness {
                activity: ChildActivity::Thinking,
                last_activity_ms: 6_000,
                wait: None,
            })
        );
    }

    /// A typed `task` call: running, with the child session id in its metadata
    /// as the event contract records it (`state.metadata.sessionId`).
    fn task_part(call_id: &str, started_at: i64, child: &str) -> Part {
        Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "task".into(),
                call_id: call_id.into(),
            },
            status: ToolStatus::Running,
            started_at: Some(started_at),
            input: Some(serde_json::json!({"description": "review"})),
            metadata: Some(serde_json::json!({"sessionId": child, "parentSessionId": "ses_parent"})),
            output: ToolOutput::default(),
        })
    }

    /// A typed V2 `subagent` call: running, with the child session id in its
    /// metadata as the current generation records it (`state.metadata.sessionID`).
    fn subagent_part(call_id: &str, started_at: i64, child: &str) -> Part {
        Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "subagent".into(),
                call_id: call_id.into(),
            },
            status: ToolStatus::Running,
            started_at: Some(started_at),
            input: Some(serde_json::json!({"agent": "general", "description": "review"})),
            metadata: Some(serde_json::json!({"sessionID": child, "status": "running"})),
            output: ToolOutput::default(),
        })
    }

    /// ADR-0054: a live task panel's title carries the child's liveness, and a
    /// later poll refreshes it from the child's new state.
    #[tokio::test]
    async fn live_task_panel_shows_the_childs_liveness() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let now = chrono::Utc::now().timestamp_millis();
        let mut backend = MockBackend::new(realistic_parts());
        let child = |started: i64, name: &str| {
            SessionTranscript::new(vec![message(
                "a_child",
                started,
                vec![tool(
                    name,
                    "call_child",
                    ToolStatus::Running,
                    Some(started),
                    Some(serde_json::json!({"command": "sleep 60"})),
                    None,
                )],
            )])
        };
        backend.given_transcript(
            "ses_child",
            vec![child(now - 5_000, "bash"), child(now - 1_000, "read")],
        );
        let parent = SessionTranscript::new(vec![message(
            "a1",
            now - 19_000,
            vec![task_part("call_task", now - 19_000, "ses_child")],
        )]);
        let (app, platform) = build_app(cfg, backend).await;
        let cards = app.cards_handle();
        let sid = "ses_parent";
        Turn::seed_card(&cards, sid, Some("om_parent")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(now - 20_000)).await;

        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &parent,
        )
        .await;
        let updates = platform.updated_cards().await;
        let first = updates.last().expect("a live flush").to_string();
        assert!(
            first.contains("bash 5s") || first.contains("bash 6s"),
            "the first poll shows the child's running tool and its elapsed time: {first}"
        );

        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &parent,
        )
        .await;
        let updates = platform.updated_cards().await;
        let second = updates.last().expect("a refresh flush").to_string();
        assert!(
            second.contains("read 1s") || second.contains("read 2s"),
            "the next poll refreshes the child's liveness: {second}"
        );
    }

    /// ADR-0054 on OpenCode 2: a live `subagent` panel carries its child
    /// session's liveness exactly as V1's `task` does, even though both the
    /// tool id (`subagent`) and the metadata's child key (`sessionID`) differ.
    #[tokio::test]
    async fn live_subagent_panel_shows_the_childs_liveness() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let now = chrono::Utc::now().timestamp_millis();
        let mut backend = MockBackend::new(realistic_parts());
        backend.given_transcript(
            "ses_v2_child",
            vec![SessionTranscript::new(vec![message(
                "a_child",
                now - 3_000,
                vec![tool(
                    "shell",
                    "call_child",
                    ToolStatus::Running,
                    Some(now - 3_000),
                    Some(serde_json::json!({"command": "cargo test"})),
                    None,
                )],
            )])],
        );
        let parent = SessionTranscript::new(vec![message(
            "a1",
            now - 9_000,
            vec![subagent_part("call_subagent", now - 9_000, "ses_v2_child")],
        )]);
        let (app, platform) = build_app(cfg, backend).await;
        let cards = app.cards_handle();
        let sid = "ses_parent";
        Turn::seed_card(&cards, sid, Some("om_parent")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(now - 10_000)).await;

        let _ = render_and_flush(
            &cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            &parent,
        )
        .await;
        let updates = platform.updated_cards().await;
        let card = updates.last().expect("a live flush").to_string();
        assert!(
            card.contains("shell 3s") || card.contains("shell 4s"),
            "the subagent panel carries its child's running tool: {card}"
        );
        assert!(
            card.contains("review"),
            "the subagent panel keeps its own input line: {card}"
        );
    }

    /// The card subtitle follows the server's live session title during a turn:
    /// OpenCode auto-renames a session after streaming, and `refresh_session_title`
    /// (called from the render poll loop) must update the card instead of leaving
    /// it on the "new session" default until restart.
    #[tokio::test]
    async fn refresh_session_title_updates_live_card_on_server_rename() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        // The server already has a final auto-generated title.
        mock.with_session_title("ses_test", "修复登录鉴权问题");
        let backend = Arc::new(mock);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        seed_entry(
            &app,
            crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/x".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            },
        )
        .await;

        // Simulate an in-flight turn whose card was captured with the OLD
        // default subtitle before the server auto-titled the session.
        let mut acc = crate::bridge::turn::state::StreamAccumulator::new("test");
        acc.reply_to_message_id = Some("msg_1".into());
        acc.session_id = Some("ses_test".into());
        {
            let mut cards = app.cards.lock().await;
            cards.insert(
                "ses_test".into(),
                crate::bridge::turn::state::CardSession::new(acc, None),
            );
        }

        // The server's live title differs → refresh must update the card subtitle.
        let refreshed = refresh_session_title(
            &app.cards_handle(),
            &app.sessions_handle(),
            &app.opencode,
            "ses_test",
        )
        .await;
        assert!(refreshed, "a server rename must refresh the card title");
        let title = app.cards.lock().await.get("ses_test").unwrap().acc.title.clone();
        assert_eq!(title, "修复登录鉴权问题 · test");
        // A second refresh with no further change must be a no-op (no churn).
        assert!(
            !refresh_session_title(
                &app.cards_handle(),
                &app.sessions_handle(),
                &app.opencode,
                "ses_test"
            )
            .await,
            "no change when the title already matches"
        );
        // No accumulator (a finished turn) → refresh is a no-op.
        app.cards.lock().await.remove("ses_test");
        assert!(
            !refresh_session_title(
                &app.cards_handle(),
                &app.sessions_handle(),
                &app.opencode,
                "ses_test"
            )
            .await
        );
    }

    /// ADR-0023: when the server auto-titles a session MID-TURN, the render
    /// tick that refreshes the live card's title must ALSO patch the topic
    /// cover card — the chat-list entry updates as early as the title agent
    /// finishes, not only when the turn completes.
    #[tokio::test]
    async fn refresh_session_title_syncs_cover_card_mid_turn() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.with_session_title("ses_test", "修复登录鉴权问题");
        let (app, platform) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

        seed_entry(
            &app,
            crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/x".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("om_seed".into()),
                topic_root: Some("om_cover".into()),
                variant: None,
            },
        )
        .await;
        seed_cover_title(&app, "ses_test", "cola").await;
        // An in-flight turn whose card was captured with the OLD subtitle
        // (before the server auto-titled the session).
        let mut acc = crate::bridge::turn::state::StreamAccumulator::new("test");
        acc.reply_to_message_id = Some("msg_1".into());
        acc.session_id = Some("ses_test".into());
        {
            let mut cards = app.cards.lock().await;
            cards.insert(
                "ses_test".into(),
                crate::bridge::turn::state::CardSession::new(acc, None),
            );
        }

        let refreshed = refresh_session_title(
            &app.cards_handle(),
            &app.sessions_handle(),
            &app.opencode,
            "ses_test",
        )
        .await;
        assert!(refreshed, "the mid-turn title change must refresh the card");

        let calls = platform.calls.lock().await.clone();
        let patched = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                    Some(card.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !patched.is_empty(),
            "the cover card must be patched on the same tick, got {calls:?}"
        );
        assert!(
            patched.last().unwrap().contains("修复登录鉴权问题"),
            "cover card must show the auto-title: {:?}",
            patched.last()
        );
        assert_eq!(
            app.core
                .cover_titles
                .lock()
                .await
                .get("ses_test")
                .map(|c| c.title.clone()),
            Some("修复登录鉴权问题".to_string())
        );
    }

    /// #183: every panel is stamped with its part's own server start time (local
    /// `HH:MM`) and the card header carries the turn's date (`MM-DD`). The epochs
    /// are constructed from local wall times, so the expected strings hold in any
    /// test-machine timezone — and the turn crossing midnight proves the header
    /// date is the turn's, not the render moment's. The header date reads the
    /// SERVER anchor (the Turn anchor's time), the accumulator's only clock (#190).
    #[test]
    fn panel_times_and_header_date_come_from_part_epochs() {
        use crate::bridge::turn::state::StreamAccumulator;
        use crate::feishu::card::CardState;
        use crate::feishu::card::test_local_ms;

        let turn_started = test_local_ms(2026, 9, 16, 23, 58);
        let reasoning_start = test_local_ms(2026, 9, 17, 0, 3);
        let tool_start = test_local_ms(2026, 9, 17, 0, 5);

        let mut acc = StreamAccumulator::new("proj");
        acc.turn_anchor = Some(turn_anchor(turn_started));
        render_parts(
            &mut acc,
            &[
                reasoning_at("thinking", reasoning_start),
                tool(
                    "bash",
                    "call_1",
                    ToolStatus::Running,
                    Some(tool_start),
                    Some(serde_json::json!({"command": "sleep 2"})),
                    None,
                ),
            ],
        );
        let running = acc.build_card().to_string();
        assert!(
            running.contains("💭 推理过程 · 00:03"),
            "reasoning time missing: {running}"
        );
        assert!(
            running.contains("⏳ bash · 00:05"),
            "running tool time missing: {running}"
        );

        // Completing the tool re-renders the panel in place: its start time must
        // stay, not slide to the completion moment.
        render_parts(
            &mut acc,
            &[tool(
                "bash",
                "call_1",
                ToolStatus::Completed,
                Some(tool_start),
                Some(serde_json::json!({"command": "sleep 2"})),
                Some("done"),
            )],
        );
        acc.card_state = CardState::Done;
        let done = acc.build_card();
        let text = done.to_string();
        assert!(
            text.contains("✅ bash · 00:05"),
            "start time lost on completion: {text}"
        );
        assert!(
            !text.contains("00:07"),
            "completion time leaked into the panel: {text}"
        );
        assert_eq!(
            done["header"]["subtitle"]["content"].as_str().unwrap(),
            "proj · 09-16",
            "the header must carry the turn's date, not the parts'"
        );
    }
}
