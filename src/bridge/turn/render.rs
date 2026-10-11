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
    FileContent, MessageId, Part, SessionTranscript, TaskRetirementEnding, ToolStatus, TurnAnchor, Wake,
    WakeSource,
};
use crate::bridge::core::SESSION_INFO_TIMEOUT;
use crate::bridge::handles::{CardsHandle, RequestsHandle, SessionsHandle, TurnHandles};
use crate::bridge::span;
use crate::bridge::turn::state;
use crate::bridge::turn::state::{LedgerCadence, PartSource, RenderedPart, StreamAccumulator};
use crate::config::ThreadKey;
use crate::feishu::card::ledger::{TaskCompletionEntry, TaskEnding, TaskKind, TaskOutput};
use crate::feishu::card::tool_render::{ChildActivity, FileDelivery, TaskLiveness};

use super::Turn;
use super::flush::FlushOutcome;

/// How long one completion entry's output read may take (spec #588, #593): the
/// entry renders once, so the read is spent exactly once, but it must not park
/// a render pass on a half-open connection. Matches the flows' own request
/// bound (`ExternalFlow::request_timeout_ms`).
const ENTRY_OUTPUT_READ_TIMEOUT_MS: u64 = 30_000;

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
    if card.acc.title() == fresh {
        return false;
    }
    tracing::info!(
        "session {} title updated: {:?} -> {:?}",
        session_id,
        card.acc.title(),
        fresh
    );
    card.acc.set_title(&fresh);
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
            !text.text.is_empty() && !acc.has_rendered_part(&RenderedPart::Text(text.text.clone()))
        }
        Part::Reasoning(reasoning) => {
            !reasoning.text.is_empty()
                && !acc.has_rendered_part(&RenderedPart::Reasoning(reasoning.text.clone()))
        }
        Part::Tool(call) => {
            // The current panel IS the call's rendered revision: an update
            // (running → completed, a late output, or a todowrite list
            // rewritten with same-length items) differs from it and
            // re-renders; an unchanged poll skips. The dedup rule — the
            // `todowrite` tail with its clock, or the `tools` map — lives in
            // the accumulator ([`StreamAccumulator::tool_panel_current`]).
            //
            // The comparison is against the typed call BEFORE the panel is
            // built: an unchanged poll (the common case) must not deep-copy
            // the call's raw payloads into a panel only to drop it.
            !acc.tool_panel_current(call)
        }
        // Step boundaries, patches, file attachments and part kinds this build
        // does not model render nothing — there is no content to add or dedupe.
        Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::File(_) | Part::Other(_) => false,
    }
}

/// Render one typed part into the accumulator, applying the dedup rules
/// ([`renders_part`]): text and reasoning are tracked by their content
/// (OpenCode part payloads carry NO `id`, AGENTS.md #9), and a tool call
/// re-renders exactly when its typed panel revision changed. A part the SEED
/// already delivered in this very message is skipped whatever the read's
/// positions say (spec #561, review #569): the resolved positions can shift
/// under a later read, and the same content in ANOTHER message — the new
/// Turn's own answer — still renders. `source` is the part's own position in
/// the transcript, recorded on text/reasoning timeline
/// entries for the Rendered Cursor frontier (spec #561); callers without one
/// (synthetic batches, the seeded live-set join) pass `None`. Returns true when
/// the part rendered (not skipped as duplicate/empty).
fn render_part(acc: &mut StreamAccumulator, source: Option<PartSource>, part: &Part) -> bool {
    if let Some(source) = source.as_ref()
        && acc.part_seeded_delivered(&source.message_id, part)
    {
        return false;
    }
    if !renders_part(acc, part) {
        return false;
    }
    match part {
        Part::Text(text) => {
            acc.mark_rendered(RenderedPart::Text(text.text.clone()));
            if let Some(rewritten) = source
                .as_ref()
                .filter(|source| acc.source_rewritten(source, &text.text))
            {
                // The server rewrote the part: replace its entry run (spec
                // #561, review #569) — appending the new snapshot would
                // double-count the part and leave the cursor digestless.
                acc.replace_text_run(rewritten, &text.text, text.started_at);
            } else {
                let (source, chunk) = source_chunk(acc, source, &text.text, None);
                acc.push_text_from(text.started_at, source, &chunk);
            }
            acc.mark_streaming();
        }
        Part::Reasoning(reasoning) => {
            acc.mark_rendered(RenderedPart::Reasoning(reasoning.text.clone()));
            if let Some(rewritten) = source
                .as_ref()
                .filter(|source| acc.source_rewritten(source, &reasoning.text))
            {
                acc.replace_reasoning_run(rewritten, &reasoning.text, reasoning.started_at);
            } else {
                let (source, chunk) = source_chunk(
                    acc,
                    source,
                    &reasoning.text,
                    Some(crate::feishu::card::REASONING_TEXT_CAP),
                );
                acc.push_reasoning_from(reasoning.started_at, source, &chunk);
            }
            acc.mark_reasoning();
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
                acc.set_todo_panel(panel, call.started_at);
            } else {
                acc.push_tool_from(call.started_at, &call.identity.call_id, panel, source);
            }
            if call.status == ToolStatus::Running {
                acc.mark_streaming();
            }
        }
        Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::File(_) | Part::Other(_) => {
            return false;
        }
    }
    // card_state / running-tool changes reset the header phase timer.
    acc.refresh_phase();
    true
}

/// The chunk an ordinary source-carrying text/reasoning render still owes the
/// card (spec #561, Codex review on PR #569): the server re-sends a grown part
/// WHOLE, so only the characters this accumulator has not delivered yet are
/// pushed — appended to the part's existing entry, or as its next chunk. The
/// returned source carries no offset (the timeline already holds everything
/// before the chunk, so the part's entries stay disjoint and the Rendered
/// Cursor's extent stays the part's own character count) but it does carry the
/// delivered prefix's digest (review #569: the same-slot replacement guard a
/// part with no server start time needs). A synthetic push (`source: None`) and
/// a part's first render push whole, exactly as the content-keyed dedup always
/// did. A REWRITE never reaches this function: [`render_part`] replaces the
/// part's entry run instead (the old content is gone from the read), and the
/// offset-entry case — a seeded part this accumulator only holds the tail of —
/// keeps no digest, so resolution falls back rather than trust an ambiguous
/// prefix.
fn source_chunk(
    acc: &StreamAccumulator,
    source: Option<PartSource>,
    text: &str,
    cap: Option<usize>,
) -> (Option<PartSource>, String) {
    let Some(source) = source else {
        return (None, text.to_string());
    };
    let rendered = acc.source_rendered(&source);
    // The chunk to push and the delivered prefix's digest, when it is
    // unambiguous: a part's FIRST render pushes the whole snapshot, an append
    // pushes its tail, and both leave the new snapshot as the delivered prefix.
    // A seeded offset entry keeps no digest — resolution then falls back rather
    // than trust an ambiguous prefix (a rewrite never reaches here: it replaced
    // the part's run). `cap` is the entry's visible-character cap (a reasoning
    // element's): the digest then covers exactly the delivered prefix the card
    // showed, never characters beyond it (spec #561, review #569).
    let shown = |chars: usize| match cap {
        Some(cap) => chars.min(cap),
        None => chars,
    };
    let merging = matches!(&rendered, Some(rendered) if !rendered.is_empty() && text.starts_with(rendered));
    let (chunk, fresh_source) = match &rendered {
        Some(rendered) if merging => (text[rendered.len()..].to_string(), false),
        _ => (text.to_string(), !acc.has_source(&source)),
    };
    // The digest covers EXACTLY the delivered prefix the card shows after this
    // push (spec #561, review #569): an entry merged with its predecessor holds
    // the whole snapshot — so a part growing past the cap fingerprints
    // `min(part length, cap)`, never `delivered + cap` — while a fresh entry
    // holds only its chunk, on top of the offset a seeded entry carries.
    let entry_chars = if merging {
        text.chars().count()
    } else {
        chunk.chars().count()
    };
    let delivered_before = if merging {
        acc.source_delivered_before(&source)
    } else {
        0
    };
    let digest = (fresh_source || merging).then(|| {
        let len = (delivered_before + shown(entry_chars)).min(text.chars().count());
        crate::bridge::chain::cursor_prefix_digest(&text.chars().take(len).collect::<String>())
    });
    (
        Some(PartSource {
            message_id: source.message_id,
            index: source.index,
            delivered_before: 0,
            prefix_digest: digest,
        }),
        chunk,
    )
}

/// Render a batch of typed parts into the accumulator, skipping anything
/// already rendered (same dedup as the poll loop). Returns true if anything
/// new was rendered. Test-only since the async-native Turn reconciles from the
/// settled transcript instead of a prompt response's inline parts (ADR-0056).
#[cfg(test)]
pub(super) fn render_parts(acc: &mut StreamAccumulator, parts: &[Part]) -> bool {
    let mut rendered_any = false;
    for part in parts {
        if render_part(acc, None, part) {
            rendered_any = true;
        }
    }
    rendered_any
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
    // The decoder reports an absent provider as an empty string; an empty
    // value leaves the last known provider in place.
    acc.apply_footer_model(&model.model_id, &model.provider_id, model.variant.as_deref());
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
        output: None,
    })
}

/// What a [`PlannedEntry`]'s commit announces (spec #588, review PR #595): the
/// exactly-once class decides whether the durable Wake Watermark may move.
pub(super) enum PlannedAnnounce {
    /// A real Wake's completion at this `created_ms`: the exactly-once mark
    /// plus the durable Wake Watermark stage (ADR-0061), so a restart never
    /// re-announces a Wake a card already showed.
    Wake { created_ms: i64 },
    /// A synthetic retirement — a runtime verdict, a child's own terminal
    /// transcript, or the user's cleanup (key `runtime:<call_id>`): the
    /// exactly-once mark alone. Nothing durable is staged, so a restart still
    /// continues a genuinely un-announced late Wake at or below the synthetic
    /// clock (#590: "a late Wake for a cleared task behaves as before; nothing
    /// is suppressed").
    Synthetic,
}

/// One completion entry a ledger read has planned but not yet inserted (spec
/// #588, #593): every exactly-once gate the insertion applies has passed, but
/// the shell output read is still owed. A venue plans under a brief read of the
/// card, spends the reads OUTSIDE the cards lock ([`read_planned_outputs`]) and
/// commits the surviving plan under the lock ([`commit_planned_entries`]) —
/// entries render at most once, so each spends at most one read, and a plan a
/// racing render beat to the announce is simply not committed.
pub(super) struct PlannedEntry {
    /// The exactly-once announce key (the Wake's id / `runtime:<call_id>`).
    key: String,
    /// What the commit announces: a real Wake's completion additionally stages
    /// the durable Wake Watermark (ADR-0061), a synthetic retirement does not.
    announce: PlannedAnnounce,
    /// The timeline insert key.
    at: Option<i64>,
    /// The directory the shell output read routes under: the card's own work
    /// directory, when it carries one.
    directory: Option<String>,
    entry: TaskCompletionEntry,
}

/// Plan the completion entries a ledger read owes (spec #593): the Wakes whose
/// resumed work this render is about to show and the Background Tasks a runtime
/// reconciliation retired without a Wake (issue #454) — or the user's cleanup
/// retired (spec #588, #590). Every gate the insertion applies is checked here
/// (the anchor, the durable Wake floor, the announce set, the
/// `runtime:<call_id>` synthetic key) but NOTHING is mutated: a caller can plan
/// under a short lock, read each shell's output tail outside it and commit the
/// plan the gates still admit. Without an anchor no entry can be placed, so no
/// plan is made at all.
pub(super) fn plan_ledger_entries(
    acc: &StreamAccumulator,
    transcript: &SessionTranscript,
    anchor: Option<&TurnAnchor>,
) -> Vec<PlannedEntry> {
    let Some(anchor) = anchor else {
        return Vec::new();
    };
    let mut plans = plan_wake_entries(acc, transcript, anchor);
    plans.extend(plan_runtime_entries(acc, transcript));
    plans
}

/// Plan the completion entry for every Wake whose resumed work this render is
/// about to show, in the read's order. A Wake that opened the card itself was
/// already announced by its 承接 line, a Wake with no server time cannot be
/// ordered (so it is skipped before its entry is built — the guard supplies
/// `created_ms`), and a Wake outside this card's Turn is not this render's
/// content — all are skipped. Each Wake marks at most once per chain
/// ([`StreamAccumulator::announce_wake`]), so a repeated poll never doubles an
/// entry.
fn plan_wake_entries(
    acc: &StreamAccumulator,
    transcript: &SessionTranscript,
    anchor: &TurnAnchor,
) -> Vec<PlannedEntry> {
    let mut plans = Vec::new();
    for wake in &transcript.wakes {
        let Some(created_ms) = wake.created_ms else {
            continue;
        };
        if created_ms < anchor.created_ms {
            continue;
        }
        // A Wake at or below the durable Watermark's floor was already
        // announced by an earlier card (spec #561, review #569; ADR-0061): its
        // completion entry is never re-inserted, and no new announcement is
        // staged for it. A strictly newer Wake renders and announces normally.
        if acc.wake_floor().is_some_and(|floor| created_ms <= floor) {
            continue;
        }
        if acc.wake_announced(wake.id.as_str()) {
            continue;
        }
        let Some(entry) = wake_completion_entry(wake, transcript, created_ms) else {
            continue;
        };
        plans.push(PlannedEntry {
            key: wake.id.as_str().to_string(),
            announce: PlannedAnnounce::Wake { created_ms },
            // Keyed at the Wake's moment, so the entry sorts before the work it
            // announces, whose server times are at/after it (the 承接 line's own
            // lesson).
            at: Some(created_ms.saturating_sub(1)),
            directory: acc.directory().map(str::to_string),
            entry,
        });
    }
    plans
}

/// Plan the completion entry for every Background Task a runtime
/// reconciliation retired without a Wake (issue #454) — or the user's cleanup
/// click retired (spec #588, #590). The retired task has already left the live
/// list, so this entry is its one record when no Wake will ever arrive: the
/// runtime's own completion time when it reported one (the `Lost` ending
/// reports none), the task's launch and identity from the read, and the label
/// joined from the originating tool part's input by `call_id` (the live row's
/// own join). A cleaned retirement carries the click's own clock and renders
/// through the same site, so the runtime's endings and the manual one cannot
/// drift apart.
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
/// the reads within the observing chain. That synthetic mark is in-memory
/// only: a retirement is not a Wake, so it stages nothing durable and never
/// advances the Wake Watermark (ADR-0061; spec #588, review PR #595).
fn plan_runtime_entries(acc: &StreamAccumulator, transcript: &SessionTranscript) -> Vec<PlannedEntry> {
    let mut plans = Vec::new();
    for retirement in &transcript.task_retirements {
        let task = &retirement.task;
        let key = format!("runtime:{}", task.tool.call_id);
        if acc.wake_announced(&key) {
            continue;
        }
        let kind = super::state::task_kind(&task.tool.name);
        let ending = match &retirement.ending {
            TaskRetirementEnding::Ended(_) => TaskEnding::RuntimeEnded,
            TaskRetirementEnding::Lost => TaskEnding::Lost,
            // The child session's own terminal transcript (#591, issue #464):
            // the runtime no longer lists the child active and its newest
            // assistant message finished — the same 结束 entry the runtime's
            // own shell ending renders.
            TaskRetirementEnding::ChildEnded => TaskEnding::RuntimeEnded,
            // The user's own cleanup (spec #588, #590): the click recorded the
            // overlay and synthesized the retirement, and the same render site
            // gives it its 🧹 entry — exactly once, through the announce key.
            TaskRetirementEnding::Cleaned => TaskEnding::Cleaned,
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
            output: None,
        };
        plans.push(PlannedEntry {
            key,
            announce: PlannedAnnounce::Synthetic,
            at: retirement.finished_at,
            directory: acc.directory().map(str::to_string),
            entry,
        });
    }
    plans
}

/// Spend each planned shell completion's one output read (spec #588, #593):
/// the record's tail, or [`TaskOutput::Unavailable`] when the read fails or
/// there is no record to read (404) — the entry then says 「输出已不可用」
/// rather than posing an empty panel. A successful read that captured NOTHING
/// is a readable-empty window instead (spec #588, review): the entry stays
/// identity-only — the entry's read is the one place the two outcomes stay
/// apart, because only this path renders the copy. Runs OUTSIDE the cards lock
/// (the reads are network) and before the commit; a plan that never commits (a
/// racing render announced it first) still spends its read at most once.
/// Endings that show no output — a subagent, the 已失联 entry, the 🧹 cleanup —
/// are left identity-only and spend nothing.
pub(super) async fn read_planned_outputs(
    backend: &Arc<dyn crate::backend::Backend>,
    plans: &mut [PlannedEntry],
) {
    for plan in plans.iter_mut() {
        if !plan.entry.shows_output() {
            continue;
        }
        let output = match plan.entry.id.as_deref() {
            Some(shell_id) => match crate::bridge::bounded_call(
                "shell output read",
                ENTRY_OUTPUT_READ_TIMEOUT_MS,
                backend.shell_output(shell_id, plan.directory.as_deref()),
            )
            .await
            {
                // A readable-empty capture: the record answered and holds
                // nothing, so no output section is rendered at all.
                Some(Ok(Some(window))) if window.text.is_empty() => None,
                Some(Ok(Some(window))) => Some(TaskOutput::Window(window)),
                _ => Some(TaskOutput::Unavailable),
            },
            // The read has no identity to ask for: the record cannot be read,
            // so the entry says so instead of posing as output.
            None => Some(TaskOutput::Unavailable),
        };
        plan.entry.output = output;
    }
}

/// One File Content the render path is about to resolve (ADR-0076): the content
/// and the content `hash` that keys the process-local upload caches.
struct InFlightUpload {
    hash: u64,
    content: FileContent,
}

/// Pre-resolve each newly rendered File Content's card delivery before the card
/// JSON is built (ADR-0076). An image within Feishu's caps is uploaded once —
/// cached process-locally by content hash, so identical bytes upload once and
/// every later PATCH reuses the key — and embedded as an `img` immediately
/// after its Tool Panel; anything else (a non-image, an image past the embed
/// caps, or an image whose embed upload failed) is uploaded once and posted as
/// exactly ONE File Message replied in-thread under the live card (#649), or
/// left `未发送` when it is past Feishu's 30MB cap or the send failed.
/// Best-effort: a failure leaves the panel's tracking line and never fails the
/// Turn. Runs OUTSIDE the cards lock for the uploads and the send (they are
/// network calls), like the planned-entry output reads. Returns whether any
/// delivery was attached — a change that owes its card PATCH.
async fn resolve_file_deliveries(cards: &CardsHandle, session_id: &str) -> bool {
    // 1. Collect the unresolved File Contents and the live card's message id
    //    under a brief lock. No cache access here, so no nested lock.
    let (pending, card_message_id) = {
        let live = cards.cards.lock().await;
        let Some(card) = live.get(session_id) else {
            return false;
        };
        (card.acc.pending_file_deliveries(), card.card_message_id.clone())
    };
    if pending.is_empty() {
        return false;
    }
    // 2. One resolution per distinct content hash, each cached process-locally
    //    (single-flight), so concurrent resolvers of one hash await ONE upload
    //    and identical bytes resolve once.
    let mut to_resolve: Vec<InFlightUpload> = Vec::new();
    let mut decided: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for delivery in &pending {
        if !decided.insert(delivery.hash) {
            continue;
        }
        to_resolve.push(InFlightUpload {
            hash: delivery.hash,
            content: delivery.content.clone(),
        });
    }
    let mut resolved: std::collections::HashMap<u64, FileDelivery> = std::collections::HashMap::new();
    for upload in to_resolve {
        if let Some(delivery) =
            resolve_file_delivery(cards, session_id, card_message_id.as_deref(), &upload).await
        {
            resolved.insert(upload.hash, delivery);
        }
    }
    // 3. Attach the deliveries under a short lock, keyed by content hash so a
    //    panel the transcript replaced mid-resolution cannot be marked with
    //    another file's state. A content left unresolved (no live card to reply
    //    under yet) is simply skipped and retried on a later poll.
    let mut live = cards.cards.lock().await;
    let Some(card) = live.get_mut(session_id) else {
        return false;
    };
    let mut changed = false;
    for delivery in pending {
        let Some(outcome) = resolved.get(&delivery.hash) else {
            continue;
        };
        card.acc
            .set_file_delivery(&delivery.call_id, delivery.index, delivery.hash, outcome);
        changed = true;
    }
    if changed {
        // A resolved delivery is rendered content, not clock churn: it owes its
        // flush and counts as progress, like a part.
        card.acc.bump_progress_mark();
    }
    changed
}

/// Resolve ONE File Content's card delivery (ADR-0076, #649): embed an image
/// within Feishu's caps, else upload the bytes once and send one File Message
/// in-thread under the live card, else `Undelivered`. Best-effort: every failure
/// degrades along the ladder (embed upload → File Message → `未发送`) and never
/// errors. `None` leaves the content unresolved for a later poll — the only
/// case is a File Message with no live card to reply under yet; the upload
/// itself is already cached, so only the send is retried.
async fn resolve_file_delivery(
    cards: &CardsHandle,
    session_id: &str,
    card_message_id: Option<&str>,
    upload: &InFlightUpload,
) -> Option<FileDelivery> {
    // Embed first: an image within the caps uploads once and every later PATCH
    // reuses the key. A non-image (`Ok(None)`) or a failed embed upload
    // degrades to the File Message path.
    let image_cell = {
        let mut cache = cards.file_images.lock().await;
        Arc::clone(
            cache
                .entry(upload.hash)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
        )
    };
    match image_cell
        .get_or_try_init(|| async { cards.feishu.upload_image(&upload.content).await })
        .await
    {
        Ok(Some(image_key)) => {
            return Some(FileDelivery::Embedded {
                image_key: image_key.clone(),
            });
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                "file content {:?} embed upload failed, degrading to a File Message: {error}",
                upload.content.name
            );
            // Cache the degradation so a later panel with the same bytes follows
            // the same File Message path instead of re-embedding.
            let _ = image_cell.set(None);
        }
    }
    // File Message: upload once, then send ONE message in-thread. `Ok(None)`
    // means past Feishu's 30MB cap — nothing was uploaded.
    let file_cell = {
        let mut cache = cards.file_uploads.lock().await;
        Arc::clone(
            cache
                .entry(upload.hash)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
        )
    };
    let file_key = match file_cell
        .get_or_try_init(|| async { cards.feishu.upload_file(&upload.content).await })
        .await
    {
        Ok(Some(file_key)) => file_key.clone(),
        Ok(None) => return Some(FileDelivery::Undelivered),
        Err(error) => {
            tracing::warn!(
                "file content {:?} file upload failed: {error}",
                upload.content.name
            );
            return Some(FileDelivery::Undelivered);
        }
    };
    // The send replies under the live card; without one, leave it unresolved so
    // a later poll (with the card) retries — the upload is already cached.
    let card_message_id = card_message_id?;
    // Once-guard: a content whose File Message already went out for this Session
    // never sends a second one (a cola restart may resend — accepted).
    let guard = (session_id.to_string(), upload.hash);
    if cards.file_messages_sent.lock().await.contains(&guard) {
        return Some(FileDelivery::SentAsFile);
    }
    let content = serde_json::json!({ "file_key": file_key });
    match cards
        .feishu
        .send_message_in_thread(card_message_id, "file", &content)
        .await
    {
        Ok(_) => {
            cards.file_messages_sent.lock().await.insert(guard);
            Some(FileDelivery::SentAsFile)
        }
        Err(error) => {
            tracing::warn!(
                "file content {:?} File Message send failed: {error}",
                upload.content.name
            );
            Some(FileDelivery::Undelivered)
        }
    }
}

/// Commit a planned read (spec #593): announce each entry and insert it, under
/// the cards lock. The announce is the exactly-once gate, so a plan a racing
/// render already committed is skipped — the entry renders once, wherever the
/// read met it. The plan's own class decides what it stages (spec #588, review
/// PR #595): a real Wake's completion advances the durable Wake Watermark, a
/// synthetic retirement only marks the in-memory announce set (ADR-0061).
/// Returns whether any entry was inserted.
pub(super) fn commit_planned_entries(acc: &mut StreamAccumulator, plans: Vec<PlannedEntry>) -> bool {
    let mut inserted = false;
    for plan in plans {
        let announced = match plan.announce {
            PlannedAnnounce::Wake { created_ms } => acc.announce_wake(&plan.key, created_ms),
            PlannedAnnounce::Synthetic => acc.announce_synthetic(&plan.key),
        };
        if !announced {
            continue;
        }
        acc.push_ledger_entry_at(plan.at, plan.entry);
        inserted = true;
    }
    inserted
}

/// The ledger facts a transcript read owes a card (ADR-0060): the read's
/// remaining live list — so a retired task's row leaves and the still-running
/// ones stay, on a continuation's very first payload or on a yielded card's
/// in-place refresh — and the completion entries the caller planned and read
/// ([`plan_ledger_entries`] + [`read_planned_outputs`]), committed here under
/// the same one-site primitive the live render uses. `activities` is the child
/// liveness this read gathered for the ledger's live subagents, keyed by call
/// id (spec #501) — empty on the paths that gather nothing, where a stored
/// fragment is kept rather than dropped. `now_ms` is the read's clock;
/// `cadence` is the granularity its ledger clock is compared at (the live
/// render and its Wake handover at whole minutes, the yielded refresh at whole
/// seconds — [`LedgerCadence`]).
///
/// A Wake handover calls this on the OUTGOING card before its chain splits;
/// Session Sync's in-place pass calls it on a yielded card. Returns whether the
/// card changed at all: a terminal card that did still owes its handover PATCH,
/// while one the read did not touch keeps the ending it shows, and a yielded
/// card is only PATCHed for a real change.
pub(super) fn apply_ledger_read(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    activities: &std::collections::HashMap<String, TaskLiveness>,
    now_ms: i64,
    cadence: LedgerCadence,
    plans: Vec<PlannedEntry>,
) -> bool {
    let mut changed = acc
        .set_ledger_from_read(transcript, activities, now_ms, cadence)
        .owes();
    changed |= commit_planned_entries(acc, plans);
    changed
}

/// Render the parts of this turn's assistant messages that haven't been
/// rendered yet, and refresh the live Background Task ledger from the read
/// with NO gathered child liveness (the sync callers — tests, the turn-end
/// reconcile — have no gather): every `subagent` row keeps the last fragment
/// that established one. Completion entries commit through the same plan/commit
/// primitives the async venues use, with no output reads (this function has no
/// backend): a Wake/runtime entry renders identity-only. The live render's own
/// entry is [`render_turn_parts`], whose caller applies the ledger from the
/// render's shared gather; a caller WITH a backend plans, reads and commits
/// through [`plan_finalization_entries`] + [`read_planned_outputs`] +
/// [`render_new_turn_parts_committing`], so its entries carry their output
/// tails too (spec #588, #593). Returns true if anything new was rendered.
#[cfg(test)]
pub(super) fn render_new_turn_parts(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    let rendered = render_turn_parts(acc, transcript);
    let plans = plan_ledger_entries(acc, transcript, acc.turn_anchor());
    let entries = commit_planned_entries(acc, plans);
    rendered | refresh_ledger(acc, transcript, &std::collections::HashMap::new()).owes() | entries
}

/// The completion entries a FINALIZATION read owes (spec #588, #593): capture
/// the read's Turn anchor first — the same idempotent capture
/// [`render_turn_parts`] runs, so a card whose anchor this read is the first
/// to establish can still place them — then plan every entry whose
/// exactly-once gates pass. The caller spends each entry's one output read
/// outside the cards lock ([`read_planned_outputs`]) and commits the surviving
/// plan with the final render ([`render_new_turn_parts_committing`]), exactly
/// like the live path ([`render_and_flush`]).
pub(super) fn plan_finalization_entries(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
) -> Vec<PlannedEntry> {
    acc.capture_turn_anchor(transcript);
    plan_ledger_entries(acc, transcript, acc.turn_anchor())
}

/// The finalization render, with the caller's already-planned (and already
/// output-read) entries committed under the same call: the finalization path
/// plans under a brief read of the card, spends the entries' output reads
/// OUTSIDE the cards lock and commits here, so a shell completion first seen
/// on the final read carries its tail — or 「输出已不可用」 — like every other
/// venue (spec #588, #593). The commit's announce gate keeps the entry exactly
/// once whatever racing venue announced it first.
pub(super) fn render_new_turn_parts_committing(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    plans: Vec<PlannedEntry>,
) -> bool {
    let rendered = render_turn_parts(acc, transcript);
    let entries = commit_planned_entries(acc, plans);
    rendered | refresh_ledger(acc, transcript, &std::collections::HashMap::new()).owes() | entries
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
/// The seed's live set is the exception — call identity, not time, resolves
/// it — so it reconciles before the gate ([`resolve_seeded_calls`]).
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
pub(super) fn render_turn_parts(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    acc.capture_turn_anchor(transcript);
    // The orphan gap this accumulator owes (spec #561, review #569) renders
    // first, on the first read that can place its cursor: the content was never
    // on a card, while the chain's Rendered Cursor has already advanced past
    // the delivered content that follows it — one frontier cannot express both.
    let gap_rendered = render_pending_gap_once(acc, transcript);
    // The seed's live set (spec #561) reconciles on EVERY render read, before
    // the anchor gate below: a seeded call is resolved by call identity against
    // the whole read, so it needs no Turn anchor. In the canonical restart
    // window the fresh Turn's message is queued behind the still running
    // orphan run, leaving the anchor unobserved for as long as that run lasts —
    // gating the live set on it would freeze the panel at its takeover status
    // for exactly that window. The projection's seed and the message-first
    // seed enter the same set: its live-set calls resolve by identity here, a
    // settled one joining the timeline exactly once and a running one riding
    // the live tail display-only.
    // The live set reconciles on every read — `||` would skip it whenever the
    // gap rendered (spec #561, review #569).
    let mut rendered_any = resolve_seeded_calls(acc, transcript);
    rendered_any |= gap_rendered;
    // The seed (spec #561) resolves against THIS read: its at-or-before-the-
    // frontier rule uses the read's own message order. The positions map serves
    // both walks below — the seed's own scope and the accumulator's window.
    let message_positions: std::collections::HashMap<&str, usize> = if acc.seed().is_some() {
        transcript
            .messages
            .iter()
            .enumerate()
            .map(|(pos, message)| (message.id.as_str(), pos))
            .collect()
    } else {
        std::collections::HashMap::new()
    };
    // The seed's own Turn window (spec #561, ticket #565): a seed carried onto
    // a card whose Turn is a DIFFERENT one — the message-first race — walks the
    // orphaned Turn before the accumulator's own window, so the run's final
    // undelivered tail renders as a continuation. It runs before the anchor
    // gate below for the same reason the live set does: while the new message
    // is queued behind the orphan run the anchor is unobserved for as long as
    // that run lasts.
    let mut seeded_messages: std::collections::HashSet<MessageId> = std::collections::HashSet::new();
    if let Some(scope) = seed_scope(acc) {
        rendered_any |= render_seed_scope(acc, transcript, &scope, &message_positions, &mut seeded_messages);
    }
    let Some(anchor) = acc.turn_anchor().cloned() else {
        return rendered_any;
    };
    // A merged Wake's completion entry (spec #593) is planned and committed by
    // the caller — [`plan_ledger_entries`] + [`read_planned_outputs`] +
    // [`commit_planned_entries`] — so its output read happens outside the cards
    // lock. The timeline keys the entry before the work it announces, so
    // committing it here or after the parts render is one card.
    let seed = acc.seed().cloned();
    for message in transcript.turn_for_user(&anchor).messages {
        // A message the seed's own scope already walked is done: walking it
        // again would render its frontier part's suffix twice in one pass.
        if seeded_messages.contains(&message.id) {
            continue;
        }
        // An error-card retry carries the failed attempt's baseline (#387):
        // its messages stay suppressed, so the rebuilt card streams only the
        // new attempt instead of replaying the old window. Every message this
        // attempt examines is recorded, so a later retry can suppress it too.
        if acc.baseline_suppresses(message.id.as_str()) {
            continue;
        }
        acc.observe_message(message.id.as_str());
        // Capture the answering model + token usage for the card footer.
        capture_footer_model(acc, message);
        // An in-flight step is its own assistant message and carries all-zero
        // usage until it finishes. Capturing that zero would wipe the last
        // completed step's figure and hide the footer's 📊 segment mid-turn.
        if let Some(tokens) = &message.tokens {
            let used = tokens.context_used();
            if used > 0 {
                acc.set_context_tokens(used);
            }
        }
        let message_pos = message_positions.get(message.id.as_str()).copied();
        for (index, part) in message.parts.iter().enumerate() {
            let cut = match (seed.as_ref(), message_pos) {
                (Some(seed), Some(pos)) => seed.cut(pos, index),
                _ => crate::bridge::turn::state::SeedCut::Undelivered,
            };
            match cut {
                // At or before the frontier: already delivered. It is marked
                // delivered (not rendered), so the content-keyed dedup and the
                // content-diff probe both see the earlier body's delivery. A
                // tool here stays in the display-only seeded set when it is
                // still running (its identity reconciliation keeps it current)
                // — only a position the window actually renders retires it.
                crate::bridge::turn::state::SeedCut::Delivered => acc.mark_delivered_part(&message.id, part),
                crate::bridge::turn::state::SeedCut::Frontier(delivered) => {
                    if render_seeded_part(acc, message.id.clone(), index, part, delivered) {
                        rendered_any = true;
                    }
                }
                crate::bridge::turn::state::SeedCut::Undelivered => {
                    // A seeded call the Turn's own window now renders is the
                    // Turn's own live panel again: it leaves the display-only
                    // seeded set, so the ordinary rules (the #284 live-panel
                    // guard included) apply to it exactly as they did before
                    // the seed existed.
                    if let Part::Tool(call) = part {
                        acc.retire_seeded_call(&call.identity.call_id);
                    }
                    // The part's own position identifies it for the Rendered
                    // Cursor frontier (spec #561): parts carry no id (AGENTS.md
                    // #9), so the message identity plus the ordinal is the
                    // whole position.
                    let source = PartSource::at(message.id.clone(), index);
                    if render_part(acc, Some(source), part) {
                        rendered_any = true;
                    }
                }
            }
        }
    }
    rendered_any
}

/// Render the orphan gap this accumulator owes, once (spec #561, review #569):
/// the first read whose cursor the gap's own Turn window can place renders the
/// tail, clears the in-memory pending value, and marks
/// [`StreamAccumulator::gap_rendered`] — the first confirmed write of a body
/// that includes the content clears the durable fact. A read that cannot place
/// it yet leaves the gap pending for the next one. One INFO line records the
/// landing (never content).
fn render_pending_gap_once(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    // The accumulator keeps the durable gap (its cursor advances only through
    // confirmed writes): `gap_rendered` is the one-shot render gate, so a
    // partial render still leaves the fact — and its remaining tail — visible
    // to the re-point that carries it on.
    if acc.gap_rendered() {
        return false;
    }
    let Some(gap) = acc.pending_gap().cloned() else {
        return false;
    };
    // A TRUNCATED read is a prefix (spec #561, review #569): render what it
    // shows, but NEVER treat the gap as rendered through — its end may be
    // beyond the page cap, so nothing may mark it complete and the walk runs
    // again on the next read, until a complete one shows the end.
    acc.set_gap_truncated(transcript.truncated);
    if !render_pending_gap(acc, transcript, &gap) {
        return false;
    }
    if transcript.truncated {
        return true;
    }
    acc.mark_gap_rendered();
    tracing::info!(
        "orphan gap: session {} rendered its pending tail",
        acc.session_id().unwrap_or("")
    );
    true
}

/// The gap walk itself: the orphaned Turn's window, cut at the gap's cursor —
/// the same shape the message-first seed's scope uses, but with NO source on
/// the pushed entries. The chain's Rendered Cursor already covers everything
/// that follows the gap, so a successor built from them can never offer a
/// frontier older than it (spec #561, review #569).
fn render_pending_gap(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    gap: &crate::bridge::chain::PendingGap,
) -> bool {
    let cursor = &gap.cursor;
    let anchor = &gap.anchor;
    let Some(seed) = state::CursorSeed::for_orphan_resolving(transcript, cursor, anchor) else {
        return false;
    };
    if seed.frontier.is_none() {
        return false;
    }
    let message_positions: std::collections::HashMap<&str, usize> = transcript
        .messages
        .iter()
        .enumerate()
        .map(|(pos, message)| (message.id.as_str(), pos))
        .collect();
    // The gap's window ends where the NEXT Turn begins (spec #561, review
    // #569): `turn_for_user` has no upper end, so without a bound the walk
    // would swallow every later Turn — content a later card already showed,
    // which the durable cursor advanced past, and which must never be
    // re-rendered as part of the gap. The gap carries the Turn the chain moved
    // to when it was recorded; a gap that names no bound falls back to the
    // read's next user message after the gap's anchor.
    let bound_pos = gap
        .bound
        .as_ref()
        .and_then(|bound| message_positions.get(bound.as_str()).copied())
        .or_else(|| next_user_position(transcript, &message_positions, anchor));
    let mut rendered = false;
    for message in transcript.turn_for_user(anchor).messages {
        let Some(pos) = message_positions.get(message.id.as_str()).copied() else {
            continue;
        };
        if let Some(bound_pos) = bound_pos
            && pos >= bound_pos
        {
            continue;
        }
        for (index, part) in message.parts.iter().enumerate() {
            match seed.cut(pos, index) {
                crate::bridge::turn::state::SeedCut::Delivered => {
                    acc.mark_delivered_part(&message.id, part);
                }
                crate::bridge::turn::state::SeedCut::Frontier(delivered) => {
                    // A part the walk renders registers its message as gap
                    // content (spec #561, review #569): the chain cursor must
                    // never be built from these entries, while the gap's own
                    // progress is read from them.
                    acc.mark_gap_message(message.id.clone());
                    if render_gap_part(acc, message.id.clone(), index, part, delivered) {
                        rendered = true;
                    }
                }
                crate::bridge::turn::state::SeedCut::Undelivered => {
                    acc.mark_gap_message(message.id.clone());
                    // A seeded call the gap renders is an ordinary settled
                    // panel again: it leaves the display-only seeded set,
                    // exactly as in the accumulator's own window.
                    if let Part::Tool(call) = part {
                        acc.retire_seeded_call(&call.identity.call_id);
                    }
                    let source = PartSource::at(message.id.clone(), index);
                    if render_part(acc, Some(source), part) {
                        rendered = true;
                    }
                }
            }
        }
    }
    rendered
}

/// The read position of the user message that ends a gap's window when the gap
/// names none: the first user message after the gap's anchor (spec #561, review
/// #569), in the read's own order. `None` when the read carries no later user
/// message — the gap's Turn is then the last one and nothing follows to bound.
fn next_user_position(
    transcript: &SessionTranscript,
    message_positions: &std::collections::HashMap<&str, usize>,
    anchor: &TurnAnchor,
) -> Option<usize> {
    transcript
        .messages
        .iter()
        .filter(|message| message.role == crate::backend::MessageRole::User)
        .filter_map(|message| message_positions.get(message.id.as_str()).copied())
        .filter(|pos| {
            transcript.messages[*pos]
                .time
                .as_ref()
                .is_some_and(|time| time.created > anchor.created_ms)
        })
        .min()
}

/// One gap part whose cut is its frontier: the undelivered suffix (with its
/// markdown lead), pushed with its source so the gap's own progress can be read
/// back from the body — the chain cursor skips it by message
/// ([`StreamAccumulator::gap_messages`]), so it can still never lower the
/// chain's frontier.
fn render_gap_part(
    acc: &mut StreamAccumulator,
    message_id: MessageId,
    index: usize,
    part: &Part,
    delivered: usize,
) -> bool {
    let (full, started_at, is_text) = match part {
        Part::Text(text) => (text.text.as_str(), text.started_at, true),
        Part::Reasoning(reasoning) => (reasoning.text.as_str(), reasoning.started_at, false),
        // The gap's cut resolved to a tool: render it whole — the ordinary
        // render's own dedup decides.
        _ => {
            let source = PartSource::at(message_id, index);
            return render_part(acc, Some(source), part);
        }
    };
    let full_len = full.chars().count();
    let held = acc.source_extent(&PartSource::at(message_id.clone(), index));
    let cut = held.max(delivered).min(full_len);
    let prefix: String = full.chars().take(cut).collect();
    let suffix: String = full.chars().skip(cut).collect();
    acc.mark_delivered_part(&message_id, part);
    if suffix.is_empty() {
        return false;
    }
    let lead = crate::feishu::card::sanitize::neutralize_tail(&prefix, &suffix);
    // The delivered prefix after this push: a reasoning element shows only its
    // cap, so the digest never covers characters the card cannot display
    // (spec #561, review #569).
    let delivered_len = match is_text {
        true => full_len,
        false => (cut
            + suffix
                .chars()
                .count()
                .min(crate::feishu::card::REASONING_TEXT_CAP))
        .min(full_len),
    };
    let source = PartSource {
        message_id,
        index,
        delivered_before: cut,
        prefix_digest: Some(crate::bridge::chain::cursor_prefix_digest(
            &full.chars().take(delivered_len).collect::<String>(),
        )),
    };
    if is_text {
        acc.push_text_lead(started_at, Some(source), &suffix, lead);
        acc.mark_streaming();
    } else {
        acc.push_reasoning_lead(started_at, Some(source), &suffix, lead);
        acc.mark_reasoning();
    }
    true
}

/// The seed's own Turn window, when it is not the accumulator's (spec #561,
/// ticket #565): the message-first seed carries the orphaned Turn's anchor, so
/// its walk runs in addition to — and before — the accumulator's own window.
/// The projection's seed has no scope (its Turn IS the accumulator's).
fn seed_scope(acc: &StreamAccumulator) -> Option<TurnAnchor> {
    acc.seed()
        .and_then(|seed| seed.scope.clone())
        .filter(|scope| acc.turn_anchor() != Some(scope))
}

/// Render the seed's own Turn window ([`seed_scope`]) through the cursor cut:
/// the frontier part renders its undelivered suffix, everything after it
/// renders normally, and everything at or before it is marked delivered. Every
/// message walked is recorded in `walked`, so the accumulator's own window
/// below skips it and one pass renders each part exactly once. A seed with no
/// frontier walks nothing — its live set alone resolves by identity, the
/// cursorless fallback. Returns whether content entered the card.
fn render_seed_scope(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    scope: &TurnAnchor,
    message_positions: &std::collections::HashMap<&str, usize>,
    walked: &mut std::collections::HashSet<MessageId>,
) -> bool {
    let Some(seed) = acc.seed().cloned() else {
        return false;
    };
    if seed.frontier.is_none() {
        return false;
    }
    // When the accumulator OWES the durable gap this scope is the window of
    // (spec #561, review #569), the seed's render IS the gap's render: its
    // entries are tagged so the chain cursor skips them and the gap's own
    // progress can be read back, and `gap_rendered` keeps the gap walk from
    // rendering the same tail again in-process — while the durable fact stays
    // until a confirmed write covers the gap's end.
    let gap_scope = acc
        .pending_gap()
        .is_some_and(|gap| gap.anchor.message_id == scope.message_id);
    let mut rendered = false;
    for message in transcript.turn_for_user(scope).messages {
        walked.insert(message.id.clone());
        let Some(pos) = message_positions.get(message.id.as_str()).copied() else {
            continue;
        };
        for (index, part) in message.parts.iter().enumerate() {
            match seed.cut(pos, index) {
                crate::bridge::turn::state::SeedCut::Delivered => acc.mark_delivered_part(&message.id, part),
                crate::bridge::turn::state::SeedCut::Frontier(delivered) => {
                    if gap_scope {
                        acc.mark_gap_message(message.id.clone());
                    }
                    if render_seeded_part(acc, message.id.clone(), index, part, delivered) {
                        rendered = true;
                    }
                }
                crate::bridge::turn::state::SeedCut::Undelivered => {
                    // A seeded call this walk renders is an ordinary live panel
                    // again: it leaves the display-only seeded set, exactly as
                    // in the accumulator's own window.
                    if let Part::Tool(call) = part {
                        acc.retire_seeded_call(&call.identity.call_id);
                    }
                    if gap_scope {
                        acc.mark_gap_message(message.id.clone());
                    }
                    let source = PartSource::at(message.id.clone(), index);
                    if render_part(acc, Some(source), part) {
                        rendered = true;
                    }
                }
            }
        }
    }
    if gap_scope && rendered {
        acc.mark_gap_rendered();
    }
    rendered
}

/// Render the frontier part of a projection seed (spec #561, ticket #563):
/// only its undelivered `text[delivered..]` suffix enters the card — with the
/// markdown lead its cut needs ([`crate::feishu::card::sanitize::neutralize_tail`])
/// — while the part is marked delivered up to its current end so a later
/// read's growth renders only the new suffix. Returns whether content entered
/// the card.
fn render_seeded_part(
    acc: &mut StreamAccumulator,
    message_id: MessageId,
    index: usize,
    part: &Part,
    delivered: usize,
) -> bool {
    let (full, started_at, is_text) = match part {
        Part::Text(text) => (text.text.as_str(), text.started_at, true),
        Part::Reasoning(reasoning) => (reasoning.text.as_str(), reasoning.started_at, false),
        // The seed resolved this position to text/reasoning; a tool here
        // means the read changed under the cursor — mark it delivered rather
        // than guess a suffix.
        _ => {
            acc.mark_delivered_part(&message_id, part);
            return false;
        }
    };
    let full_len = full.chars().count();
    let delivered = delivered.min(full_len);
    // The prefix the PREVIOUS card delivered is carried on this part's first
    // entry alone; a part this accumulator already holds (the frontier growing
    // under a later read) continues from what it delivered, never repeating
    // the offset (spec #561, review #569).
    let source = PartSource::at(message_id.clone(), index);
    let held = acc.source_extent(&source);
    // The suffix-only cut is valid only while the read still carries what the
    // chain delivered for the part. A live REWRITE — the read's prefix no
    // longer hashing to the held extent — replaces the part's run with the new
    // content in full: cutting at `held` would push a stray suffix and let the
    // cursor claim the replacement delivered (spec #561, review #569, ADR-0071's
    // rewrite rule).
    if held > 0 && !acc.source_prefix_holds(&source, full, held) {
        acc.mark_delivered_part(&message_id, part);
        if is_text {
            acc.replace_text_run(&source, full, started_at);
            acc.mark_streaming();
        } else {
            acc.replace_reasoning_run(&source, full, started_at);
            acc.mark_reasoning();
        }
        acc.record_seed_frontier_delivered(full_len);
        return !full.is_empty();
    }
    let (cut, before) = if held == 0 {
        (delivered, delivered)
    } else {
        (held, 0)
    };
    let prefix: String = full.chars().take(cut).collect();
    let suffix: String = full.chars().skip(cut).collect();
    // Everything up to the part's current end is delivered now: a later read
    // grows the part and renders only the growth.
    acc.mark_delivered_part(&message_id, part);
    if !suffix.is_empty() {
        let lead = crate::feishu::card::sanitize::neutralize_tail(&prefix, &suffix);
        // The delivered prefix after this push: the whole read's part for a
        // text entry, and only the characters a reasoning element can show for
        // a reasoning one (spec #561, review #569) — so its digest lets a later
        // resolution tell growth from a same-slot replacement AND never claims
        // characters the card could not display.
        let delivered_len = match is_text {
            true => full.chars().count(),
            false => (cut
                + suffix
                    .chars()
                    .count()
                    .min(crate::feishu::card::REASONING_TEXT_CAP))
            .min(full.chars().count()),
        };
        let source = PartSource {
            message_id,
            index,
            delivered_before: before,
            prefix_digest: Some(crate::bridge::chain::cursor_prefix_digest(
                &full.chars().take(delivered_len).collect::<String>(),
            )),
        };
        if is_text {
            acc.push_text_lead(started_at, Some(source), &suffix, lead);
            acc.mark_streaming();
        } else {
            acc.push_reasoning_lead(started_at, Some(source), &suffix, lead);
            acc.mark_reasoning();
        }
    }
    acc.record_seed_frontier_delivered(full_len);
    !suffix.is_empty()
}

/// The projection seed's live set (spec #561): a takeover hands the successor
/// the tool call ids whose newest delivered state was running (or, for a
/// cursorless record, the orphaned Turn's still-live calls); every render read
/// resolves each seeded identity against the WHOLE read — PAST the Turn
/// window, whose membership would drop the long-running call's message — so
/// the panel keeps the transcript's current status and output, and its
/// settlement joins the timeline at the server start key it was born with
/// (ADR-0045). It runs on every read, before the anchor gate: the set is
/// display-only and identity, not time, resolves it, so it works while the
/// fresh Turn's own message is still queued behind a busy orphan run. The
/// ordinary tool dedup applies, so a call the window ALSO renders is not
/// duplicated and a repeated read of an unchanged call is skipped. Returns
/// true when any seeded call rendered.
///
/// The identity is live-only: once the transcript settles it the panel is an
/// ordinary timeline record, so it leaves the seeded set and the end-of-turn
/// omission (`build_card_inner`, the `has_live_tools` guard) no longer sees
/// it. A seeded call still running when the Turn ends never outlives it: the
/// settled card omits it, because no renderer will ever update that `⏳` again.
fn resolve_seeded_calls(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    let mut rendered = false;
    let seeded = acc.seeded_call_ids();
    for call_id in seeded {
        // The call's typed position — the seeded panel keeps it, so once it
        // settles its timeline entry can become the Rendered Cursor frontier
        // (spec #561, review #569).
        if let Some(call) = transcript.tool_call(&call_id)
            && let Some((message_id, index)) = transcript.tool_call_position(&call_id)
            && render_part(
                acc,
                Some(PartSource::at(message_id, index)),
                &Part::Tool(call.clone()),
            )
        {
            rendered = true;
        }
        if acc.tool_settled(&call_id) {
            acc.retire_seeded_call(&call_id);
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
/// nothing keeps each row's stored fragment growing truthfully.
///
/// The read's completion entries are the caller's (spec #593): every venue
/// plans them on the same read ([`plan_ledger_entries`]), reads their output
/// tails outside the cards lock and commits them with their rows — the live
/// path must place each `🔔 shell 结束`/`已失联` entry on the very read that
/// observed it, because the process-local overlay filters every later read, and
/// the announce set keeps it exactly once whichever path renders that read.
/// This function owns the live list alone. Returns the split decision
/// ([`super::state::LedgerChange`]): a caller that only flushes reads `owes`,
/// while one telling new work from clock churn reads `rows` (#457).
fn refresh_ledger(
    acc: &mut StreamAccumulator,
    transcript: &SessionTranscript,
    activities: &std::collections::HashMap<String, TaskLiveness>,
) -> super::state::LedgerChange {
    if !acc.has_turn_anchor() {
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
        !acc.baseline_suppresses(message.id.as_str())
            && message
                .parts
                .iter()
                .any(|part| renders_part(acc, part) && !acc.part_seeded_delivered(&message.id, part))
    })
}

/// What one render pass did, for the loops that log it.
pub(crate) struct RenderStats {
    /// Parts appended to the card this pass (text/reasoning chunks).
    pub(crate) new_parts: usize,
    /// The card's cumulative text length after the pass (logging).
    pub(crate) text_len: usize,
    /// The card's cumulative reasoning length after the pass (logging).
    pub(crate) reasoning_len: usize,
    /// Whether this pass rendered turn parts the card did not already carry.
    /// The same-snapshot ending rule (#604) reads it: an ending may only be
    /// decided on a read that was rendered and shows no new part content, so a
    /// pass whose render added parts cannot settle. Completion entries are not
    /// counted: the settle decision was made on the same transcript that
    /// produced them, so a retirement does not gate.
    pub(crate) new_content: bool,
}

/// What one completed render pass did (review, PR #595): the stats the loops
/// log, and the disposition of the flush that carried this pass's committed
/// entries — the write-outcome half of ADR-0073's record-after-flush gate. A
/// caller that holds a reconcile pass reads [`Self::flush`] and commits only
/// on [`FlushOutcome::accepted`]; a permanently refused write records nothing.
pub(crate) struct RenderPass {
    /// The pass's rendering facts (logging).
    pub(crate) stats: RenderStats,
    /// What became of the flush this pass issued.
    pub(crate) flush: FlushOutcome,
}

/// What a [`render_and_flush_inner`] pass may do beyond rendering its read.
/// The two callers use exactly two modes — the axes move together — so one
/// named mode replaces the pair of booleans they used to pass.
#[derive(Clone, Copy)]
enum RenderMode {
    /// A normal streaming render: persist the captured anchor and flush when
    /// the pass owes a card write.
    Streaming,
    /// A settling drain tick (#604): render only — no anchor persist, no
    /// flush. Finalization's own render on the same read owns the card write,
    /// so the settle tick adds no second durable write and no out-of-order
    /// flush (a pending split still serves on finalization, as before).
    Settling,
}

impl RenderMode {
    /// Whether the pass persists the captured anchor on the durable record
    /// (ADR-0063).
    fn persists_anchor(self) -> bool {
        matches!(self, Self::Streaming)
    }

    /// Whether the pass may issue a card write; a settling pass leaves it to
    /// finalization.
    fn flushes(self) -> bool {
        matches!(self, Self::Streaming)
    }
}

/// [`render_and_flush`] for a drain tick that is settling (#604): it renders the
/// read into the accumulator but does not flush — finalization's own render on
/// the same read owns the card write, so the settle tick adds no second durable
/// write and no out-of-order flush (a pending split still serves on
/// finalization, exactly as before).
pub(super) async fn render_and_flush_settling(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    requests: &RequestsHandle,
    session_id: &str,
    transcript: &SessionTranscript,
) -> Option<RenderPass> {
    render_and_flush_inner(
        cards,
        sessions,
        backend,
        requests,
        session_id,
        transcript,
        RenderMode::Settling,
    )
    .await
}

/// Render the session's transcript into the streaming card and flush it when
/// something changed. The shared heart of both render loops — `render_poll_loop`
/// (cola's own prompts) and the external-message renderer
/// (`bridge::external`) — so the two never drift apart.
///
/// Returns `Some(pass)` when the accumulator is still present; `None` when it
/// vanished (the caller should stop). `pass.flush` reports what became of the
/// write the pass issued (review, PR #595): the reconcile callers commit their
/// pass only on an accepted one. The pass bumps the accumulator's
/// observable-progress mark ([`StreamAccumulator::progress_mark`], exposed as
/// `Turn::progress_mark`) as each stage renders, which is what the external
/// renderer's idle bound renews on; a pass abandoned by a timeout still
/// leaves the marks its completed stages made.
pub(super) async fn render_and_flush(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    requests: &RequestsHandle,
    session_id: &str,
    transcript: &SessionTranscript,
) -> Option<RenderPass> {
    render_and_flush_inner(
        cards,
        sessions,
        backend,
        requests,
        session_id,
        transcript,
        RenderMode::Streaming,
    )
    .await
}

async fn render_and_flush_inner(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    requests: &RequestsHandle,
    session_id: &str,
    transcript: &SessionTranscript,
    mode: RenderMode,
) -> Option<RenderPass> {
    // OpenCode auto-renames sessions after a turn; follow the server's live
    // title so the card subtitle doesn't stay on the "new session" default.
    refresh_session_title(cards, sessions, backend, session_id).await;
    let (changed, header_changed, new_parts, text_len, reasoning_len, anchor, mut plans) = {
        let mut live = cards.cards.lock().await;
        let card = live.get_mut(session_id)?;
        let before = card.acc.rendered_part_count();
        // The anchor this read establishes (or already carries) is the scope
        // both the parts and the completion entries place against: capture it
        // BEFORE planning the entries, so their output reads can run outside
        // this lock below. `render_turn_parts` re-runs the same idempotent
        // capture.
        card.acc.capture_turn_anchor(transcript);
        // The completion entries this read owes (spec #593): planned here
        // (pure), their output tails read below, committed before the flush —
        // the live path must render the retirement entry on the very read that
        // observed it.
        let plans = plan_ledger_entries(&card.acc, transcript, card.acc.turn_anchor());
        let changed = render_turn_parts(&mut card.acc, transcript);
        if changed {
            // Mark the progress NOW: a later stage of this pass may be
            // abandoned by the caller's timeout (#457), and the external
            // renderer renews from the accumulator's mark, not from a flag a
            // cancelled pass can never return.
            card.acc.bump_progress_mark();
        }
        // The Turn anchor this render captured (or already carried) plus the
        // card it belongs to: the durable live-card record's anchor is written
        // below, outside the lock (ADR-0063).
        let anchor = card.card_message_id.clone().zip(card.acc.turn_anchor().cloned());
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
            card.acc.rendered_part_count() - before,
            card.acc.text().len(),
            card.acc.reasoning().len(),
            anchor,
            plans,
        )
    };
    // Resolve every newly rendered File Content's card delivery (ADR-0076)
    // before the flush builds the card JSON: one upload per content, outside
    // the cards lock. Best-effort — a failure leaves the tracking line. An
    // attached delivery is a rendered change and owes its PATCH.
    let file_changed = resolve_file_deliveries(cards, session_id).await;
    // One output read per planned entry, OUTSIDE the cards lock (the reads are
    // network), then one short lock to announce and insert them (spec #593).
    read_planned_outputs(backend, &mut plans).await;
    let entries_changed = {
        let mut live = cards.cards.lock().await;
        match live.get_mut(session_id) {
            Some(card) => {
                let inserted = commit_planned_entries(&mut card.acc, plans);
                if inserted {
                    // An inserted entry is rendered content, not clock churn:
                    // it owes its flush and counts as progress, like a part.
                    card.acc.bump_progress_mark();
                }
                inserted
            }
            None => false,
        }
    };
    // Persist the captured anchor on the durable record (ADR-0063), so a
    // restart's reap can ask the transcript what became of this Turn's message
    // instead of probing for it. A no-op while the record names another card.
    // A caller that will finalize on this same read defers it (#604): the
    // finalization's own render names the anchor, and the extra write here
    // would be a second persist for one turn.
    if mode.persists_anchor()
        && let Some((card_message_id, anchor)) = &anchor
    {
        cards.chains.set_anchor(session_id, card_message_id, anchor);
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
                if changed {
                    card.acc.bump_progress_mark();
                }
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
            Some(card) => {
                let ledger = refresh_ledger(&mut card.acc, transcript, &liveness);
                let liveness_changed = apply_task_liveness(&mut card.acc, &panel_children, &liveness);
                // Only the ROW half is progress: a rendered elapsed/age
                // crossing its cadence (the clock half) must not renew the
                // external renderer's idle bound, or a silent task's ticking
                // age would keep its card live forever (#457).
                if ledger.rows || liveness_changed {
                    card.acc.bump_progress_mark();
                }
                (ledger, liveness_changed)
            }
            None => (super::state::LedgerChange::default(), false),
        }
    };
    let flush = if !mode.flushes() {
        // A settling render (#604) leaves the card write to finalization, which
        // renders and flushes this same read: no carrier here.
        FlushOutcome::Unwritten
    } else if changed
        || entries_changed
        || file_changed
        || header_changed
        || context_changed
        || ledger.owes()
        || liveness_changed
    {
        Turn::flush_card(cards, session_id).await
    } else {
        // Nothing this pass did owed a card write: no carrier, so nothing a
        // caller may read as accepted (review, PR #595).
        FlushOutcome::Unwritten
    };
    Some(RenderPass {
        stats: RenderStats {
            new_parts,
            text_len,
            reasoning_len,
            new_content: changed,
        },
        flush,
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
                Part::StepStart(_)
                | Part::StepFinish(_)
                | Part::Patch(_)
                | Part::File(_)
                | Part::Other(_) => {}
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
            Some(pass) => {
                let stats = pass.stats;
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
        BackgroundTask, FileContent, MessageId, MessageRole, MessageTime, ReasoningPart, StepFinish,
        StepStart, ToolCall, ToolIdentity, ToolOutput, TranscriptMessage, TurnAnchor, TurnSettle,
    };
    use crate::bridge::App;
    use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor, cursor_prefix_digest};
    use crate::bridge::test_support::{
        MockBackend, PlatformCall, RecordingPlatform, build_app, card_text, final_card, realistic_parts,
        seed_cover_title, seed_entry, test_config, test_work_dir, text_part, turn_anchor, typed_message,
    };
    use crate::bridge::turn::state::{CursorSeed, StreamAccumulator};
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
        acc.repoint_turn_anchor(&turn_anchor(0));
        assert_eq!(acc.current_phase(), Some(&HeaderPhase::Loading));

        let transcript = SessionTranscript::new(vec![message(
            "a1",
            100,
            vec![reasoning_part("Let me think"), text_part("Answer")],
        )]);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(acc.current_phase(), Some(&HeaderPhase::Streaming));
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
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        assert_eq!(acc.context_tokens(), 210_239);

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
        assert_eq!(acc.context_tokens(), 216_860);
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
        acc.set_card_state(CardState::Done);

        assert!(acc.reasoning().contains("The user is asking in Chinese."));
        assert_eq!(acc.tools().len(), 1);
        let tool = &acc.tools()["call_1"];
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
        assert_eq!(acc.tools()["call_2"].output().as_deref(), Some("fn main() {}"));
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
        acc.set_card_state(CardState::Done);
        render_parts(&mut acc, &running);
        assert_eq!(
            acc.card_state(),
            &CardState::Streaming,
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
        acc.set_card_state(CardState::Done);
        render_parts(&mut acc, &completed);
        assert_eq!(
            acc.card_state(),
            &CardState::Done,
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
        let out = acc.tools()["call_patch"].output().unwrap();
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
        acc.repoint_turn_anchor(&turn_anchor(anchor));

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
        assert!(acc.reasoning().contains("Let me think"));
        assert_eq!(acc.tools().len(), 1);
        assert_eq!(acc.rendered_part_count(), 1);
        assert!(!acc.text().contains("question"));
        assert!(!acc.reasoning().contains("old reasoning"));

        assert!(!render_new_turn_parts(&mut acc, &transcript));
    }

    /// The live Turn card does NOT render a skill fold (spec #652, ticket #655,
    /// acceptance reversal): a `/skill` turn's card carries its text/reasoning/
    /// tool panels only — the loaded-skill fold lives on the dedicated small
    /// card, never here.
    #[test]
    fn a_skill_turn_renders_no_fold_on_the_live_card() {
        let mut acc = StreamAccumulator::new("test");
        acc.set_cola_message_id("msg_cola_1");
        acc.set_skills(&[crate::backend::PromptSkill {
            id: "implement-spec".into(),
            name: "Implement Spec".into(),
        }]);
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_1",
                MessageRole::User,
                Some(2000),
                vec![text_part("/skill implement-spec 644")],
            ),
            message("a1", 3000, vec![text_part("done")]),
        ]);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        assert!(
            !acc.build_card().to_string().contains("已加载技能"),
            "the live Turn card must not carry the skill fold"
        );
    }

    /// A user message's File Content never reaches the live card (ADR-0076):
    /// the part kind is a renderer no-op (`renders_part`), so a turn whose user
    /// message carries an attachment renders only its assistant reply — no
    /// `img` element, no name/mime/payload line. The record line and the
    /// delivery are the Snapshot/Platform surfaces' (tickets #647/#649); this
    /// pins that neither leaks onto the live card.
    #[test]
    fn a_user_file_part_renders_nothing_on_the_live_card() {
        let content = FileContent::decode("data:image/png;base64,QUJD", Some("image/png"), Some("shot.png"))
            .expect("an inline payload is a File Content");

        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));

        // The turn: the user message that anchors it — its text beside the
        // attachment — and the assistant reply, the only part that renders.
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "u1",
                MessageRole::User,
                Some(0),
                vec![text_part("看看这个"), Part::File(content.clone())],
            ),
            message("a1", 100, vec![text_part("看到了")]),
        ]);
        assert!(render_new_turn_parts(&mut acc, &transcript));
        // The render loop's own arm, walked directly for the File part: the
        // part adds nothing, exactly as it does from the transcript read.
        assert!(!render_parts(&mut acc, &[Part::File(content)]));

        let rendered = acc.build_card().to_string();
        assert!(rendered.contains("看到了"), "the turn's reply still renders");
        assert!(
            !rendered.contains(r#""img""#),
            "a user attachment must add no image element"
        );
        assert!(
            !rendered.contains("shot.png"),
            "the file's name must stay off the live card"
        );
        assert!(
            !rendered.contains("image/png"),
            "the file's mime must stay off the live card"
        );
        assert!(
            !rendered.contains("QUJD"),
            "the inline payload must stay off the live card"
        );
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
        acc.set_cola_message_id("msg_cola_1");
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
            acc.turn_anchor(),
            Some(&TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 2000,
            }),
            "the anchor is the message's identity together with its server time"
        );
        assert!(
            acc.reasoning().contains("还在研究"),
            "the in-flight message's reasoning must render live: {:?}",
            acc.reasoning()
        );
        assert_eq!(
            acc.tools()["call_task"].status(),
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
        acc.repoint_turn_anchor(&turn_anchor(anchor));
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
        assert_eq!(acc.tools()["call_task"].status(), &ToolStatus::Running);

        // Completed AFTER the anchor: the settled panel still renders.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(Some(2_500), ToolStatus::Completed, "research done")
        ));
        assert_eq!(acc.tools()["call_task"].status(), &ToolStatus::Completed);
        assert_eq!(
            acc.tools()["call_task"].output().as_deref(),
            Some("research done")
        );

        // Re-fetching the same settled state must not duplicate.
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript(Some(2_500), ToolStatus::Completed, "research done")
        ));
    }

    // --- The takeover seed's live set (spec #561; ADR-0068's successor) -----
    //
    // A tool that settled while cola was down is deliberately out of scope
    // here: the faithful no-duplicate/no-omission restore of missed content is
    // the cursor's projection (tickets #563/#564), not a live-set fallback's.

    /// Regression for the in-flight window: a running call inside it already
    /// renders onto the successor through the ordinary transcript render — no
    /// seed needed — and its completion settles it into the timeline exactly
    /// once. The probe that pinned this behavior becomes the regression the
    /// live set must not break.
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
        acc.set_cola_message_id("msg_cola_new");
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Running, None)
        ));
        assert_eq!(acc.tools()["call_sleep"].status(), &ToolStatus::Running);
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
        assert_eq!(acc.tools()["call_sleep"].status(), &ToolStatus::Completed);
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("slept").count(), 1, "settled once: {text}");
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, Some("slept"))
        ));
    }

    /// Spec #561's live-set fallback (ADR-0068's carry, retired into it): a
    /// call older than the in-flight window is dropped by the successor's own
    /// render, so the takeover seeds the orphan Turn's running call — the
    /// successor's live tail shows `⏳`, and its completion renders exactly
    /// once, joining the timeline at its server start key. A later transcript
    /// render of the same call does not duplicate it.
    #[test]
    fn a_stale_inflight_tool_is_seeded_and_settles_once_into_the_timeline() {
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
        acc.set_cola_message_id("msg_cola_new");
        // The takeover seeds the orphan Turn's running calls before the prompt.
        assert_eq!(
            seed_live_calls(&mut acc, &transcript(ToolStatus::Running, None), &orphan),
            1,
            "the orphan's running call is the seed's live set"
        );
        render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
        assert_eq!(acc.tools()["call_sleep"].status(), &ToolStatus::Running);
        let (card, full) = acc.build_card_with_split();
        assert!(!full, "a live card with only a tail panel must not split");
        assert!(
            card_text(&card).contains("⏳ shell"),
            "the seeded call rides the successor's live tail: {card}"
        );

        // The tool completes while its message stays stale: the seeded
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
        assert!(!acc.tools()["call_sleep"].is_live());
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

    /// Spec #561's live set is every LIVE call — `pending` as well as
    /// `running` (`ToolStatus::is_live`) — so a pending orphan is seeded into
    /// the successor's live tail exactly like a running one, and its
    /// settlement reconciles exactly once at its server start key.
    #[test]
    fn a_pending_orphan_call_is_seeded_and_settles_once() {
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
        acc.set_cola_message_id("msg_cola_new");
        // The takeover seeds the orphan Turn's pending call: `pending` is live,
        // so it is part of spec #561's live set.
        assert_eq!(
            seed_live_calls(&mut acc, &transcript(ToolStatus::Pending, None), &orphan),
            1,
            "a pending orphan call is seeded like a running one"
        );
        // The first render resolves the seeded identity into the live tail.
        render_new_turn_parts(&mut acc, &transcript(ToolStatus::Pending, None));
        assert_eq!(acc.tools()["call_sleep"].status(), &ToolStatus::Pending);
        let (card, full) = acc.build_card_with_split();
        assert!(!full, "a live card with only a tail panel must not split");
        assert!(
            card_text(&card).contains("⏳ shell"),
            "the seeded pending call rides the successor's live tail: {card}"
        );

        // The call completes while its message stays stale: the seeded
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
        assert!(!acc.tools()["call_sleep"].is_live());
        assert!(
            acc.seeded_calls().is_empty(),
            "a settled seeded call leaves the display-only live set"
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

    /// The live set is scoped to the orphan Turn's projection: an older,
    /// unrelated turn's stale `running` part is never resurrected on the
    /// successor.
    #[test]
    fn only_the_orphan_turns_running_calls_enter_the_live_set() {
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

        let seeded: Vec<String> = transcript
            .turn_running_tools(&orphan)
            .iter()
            .map(|call| call.identity.call_id.clone())
            .collect();
        assert_eq!(
            seeded,
            ["call_sleep"],
            "only the orphan Turn's running call enters the live set"
        );
    }

    /// A killed run's seeded call reconciles to the transcript's final status
    /// on every render read — beyond the Turn window — and never outlives the
    /// Turn: the panel invents no ending while the transcript says `running`,
    /// and the successor's own settle decision is untouched by it. When the
    /// transcript finally records a status, the panel takes it and settles.
    #[test]
    fn a_seeded_call_beyond_the_window_reconciles_on_every_render_read() {
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
        acc.set_cola_message_id("msg_cola_new");
        seed_live_calls(&mut acc, &transcript(ToolStatus::Running, None), &orphan);
        // Repeated reads while the transcript says running: the seeded panel
        // stays truthfully live and never invents an ending.
        for _ in 0..3 {
            render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
            assert!(
                acc.tools()["call_sleep"].is_live(),
                "a killed run's seeded call keeps the transcript's running status"
            );
        }
        assert_eq!(
            transcript(ToolStatus::Running, None).settle(Some(&new_anchor)),
            TurnSettle::Complete,
            "the seeded panel is display-only: the Turn's settle decision is unchanged"
        );

        // The transcript finally records the failed run: the panel reconciles.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Error, Some("killed"))
        ));
        assert!(!acc.tools()["call_sleep"].is_live());
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches("killed").count(), 1, "reconciled once: {text}");
    }

    /// The canonical restart window (spec #561): the fresh Turn's message is
    /// queued behind the still-running orphan run, so the transcript carries no
    /// anchor for as long as that run lasts. A seeded call is resolved by call
    /// identity against the whole read, so its completion reconciles into the
    /// timeline anyway — while the anchor is unobserved — exactly once; the
    /// later anchor capture neither duplicates nor re-orders it.
    #[test]
    fn a_seeded_call_reconciles_before_the_turn_anchor_is_observed() {
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
        acc.set_cola_message_id("msg_cola_new");
        assert_eq!(
            seed_live_calls(&mut acc, &anchorless(ToolStatus::Running, None), &orphan),
            1
        );
        assert_eq!(acc.turn_anchor(), None);

        // The call completes while the fresh message is still absent: the
        // seeded identity reconciles anyway, joining the timeline at its
        // server start key.
        assert!(render_new_turn_parts(
            &mut acc,
            &anchorless(ToolStatus::Completed, Some("slept"))
        ));
        assert_eq!(acc.turn_anchor(), None, "still no anchor to gate on");
        assert!(!acc.tools()["call_sleep"].is_live());
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
            acc.turn_anchor(),
            Some(&TurnAnchor {
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

    /// A seeded call the Turn's own window renders is the Turn's own live
    /// panel again: it leaves the display-only live set, so the ordinary
    /// live-panel rules (#284's guard included) apply exactly as before the
    /// seed existed.
    #[test]
    fn an_in_window_render_readopts_a_seeded_call() {
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
        acc.set_cola_message_id("msg_cola_new");
        assert_eq!(
            seed_live_calls(&mut acc, &transcript(ToolStatus::Running, None), &orphan),
            1
        );
        assert!(acc.seeded_calls().contains("call_sleep"));

        render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
        assert!(
            acc.seeded_calls().is_empty(),
            "the Turn's own window render owns the call now, not the seed"
        );
    }

    /// The successor's own reads rebuild the todo list, so a running
    /// `todowrite` never enters the seed's live set (spec #561's fallback: the
    /// todo list, the ledger and the interaction blocks stay out).
    #[test]
    fn a_running_todowrite_never_enters_the_live_set() {
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: 1_000_000,
        };
        let transcript = SessionTranscript::new(vec![message_in_flight(
            "a_todo",
            1_000_000 + 5_000,
            None,
            vec![Part::Tool(ToolCall {
                identity: ToolIdentity {
                    name: "todowrite".into(),
                    call_id: "call_todo".into(),
                },
                status: ToolStatus::Running,
                started_at: Some(1_000_000 + 5_000),
                input: Some(serde_json::json!({"todos": []})),
                metadata: None,
                output: ToolOutput::default(),
            })],
        )]);

        let seed = CursorSeed::live_calls_only(&transcript, &orphan);
        assert!(
            seed.live_calls.is_empty(),
            "the successor's own reads rebuild the todo list"
        );
        let mut acc = StreamAccumulator::new("proj");
        acc.seed_projection(&RenderedCursor::default(), seed);
        assert!(acc.tools().is_empty() && acc.seeded_calls().is_empty());
    }

    /// A killed run's seeded call does not outlive the Turn (spec #561): while
    /// a live renderer owns the card the panel rides the tail and reconciles,
    /// but once the card settles no renderer will ever update it again, so a
    /// still-running seeded panel is not built — no permanent `⏳` on the
    /// final card. A seeded call that settled before the end is a timeline
    /// entry and still renders exactly once.
    #[test]
    fn a_still_running_seeded_call_is_not_built_once_the_card_is_settled() {
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
        acc.set_cola_message_id("msg_cola_new");
        seed_live_calls(&mut acc, &transcript(ToolStatus::Running, None), &orphan);
        render_new_turn_parts(&mut acc, &transcript(ToolStatus::Running, None));
        assert!(
            card_text(&acc.build_card_with_split().0).contains("⏳ shell"),
            "a live renderer owns the card, so the seeded panel rides the tail"
        );

        // The Turn ends with the call still running (the killed-run case): the
        // settled card omits the panel — the `⏳` never outlives the Turn.
        acc.set_card_state(CardState::Done);
        let text = card_text(&acc.build_card_with_split().0);
        assert!(
            !text.contains("⏳ shell"),
            "the settled card must not keep a permanent running panel: {text}"
        );
        // A yielded card has no renderer either: same rule.
        acc.set_card_state(CardState::Waiting);
        assert!(
            !card_text(&acc.build_card_with_split().0).contains("⏳ shell"),
            "a card no renderer owns never shows a still-running seeded panel"
        );

        // A completion that landed before the end is a timeline record and
        // still renders on the settled card, exactly once.
        acc.set_card_state(CardState::Streaming);
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, Some("slept"))
        ));
        acc.set_card_state(CardState::Done);
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(
            text.matches("slept").count(),
            1,
            "a settled seeded call still renders on the final card: {text}"
        );
    }

    /// The cursorless record's fallback seed (spec #561, ticket #565): the
    /// orphaned Turn's still-live calls enter the seed's live set and the
    /// accumulator, exactly as ADR-0068's carry once did. Returns how many
    /// calls were seeded.
    fn seed_live_calls(
        acc: &mut StreamAccumulator,
        transcript: &SessionTranscript,
        orphan: &TurnAnchor,
    ) -> usize {
        let seed = CursorSeed::live_calls_only(transcript, orphan);
        let count = seed.live_calls.len();
        acc.seed_projection(&RenderedCursor::default(), seed);
        count
    }

    /// The message-first race (spec #561, ticket #565): the takeover seeds the
    /// accumulator with the ORPHANED Turn's own anchor as the seed's scope, so
    /// the render walks the orphan's window — its undelivered tail, cut at the
    /// confirmed frontier — before the new Turn's content, even while the new
    /// message is still queued and the accumulator has no anchor.
    #[test]
    fn a_message_first_seed_renders_the_orphans_tail_then_the_new_answer() {
        let new_anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: new_anchor - 31 * 60_000,
        };
        let prefix = "第一段回答。";
        let tail = "没送达的尾巴。";
        let full = format!("{prefix}{tail}");
        // The canonical window: the new message is queued behind the orphan
        // run, so the read carries no anchor for the new Turn.
        let queued = |text: &str| {
            SessionTranscript::new(vec![message_in_flight(
                "a_orphan",
                new_anchor - 30 * 60_000,
                None,
                vec![text_at(text, new_anchor - 30 * 60_000)],
            )])
        };
        let answered = |text: &str| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_new",
                    MessageRole::User,
                    Some(new_anchor),
                    vec![text_part("新的问题")],
                ),
                message_in_flight(
                    "a_orphan",
                    new_anchor - 30 * 60_000,
                    None,
                    vec![text_at(text, new_anchor - 30 * 60_000)],
                ),
                message("a_new", new_anchor + 1_000, vec![text_part("新回答")]),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.set_cola_message_id("msg_cola_new");
        // The takeover's seed: the frontier inside the orphan's answer, the
        // orphan's own Turn as the scope, no live calls.
        let cursor = projection_cursor_at(
            "a_orphan",
            0,
            CursorPartKind::Text,
            Some(new_anchor - 30 * 60_000),
            prefix,
            &[],
        );
        let seed =
            CursorSeed::for_orphan_resolving(&queued(&full), &cursor, &orphan).expect("the cursor resolves");
        acc.seed_projection(&cursor, seed);

        // Queued: the seed's own scope renders the tail with no anchor
        // observed.
        assert!(render_new_turn_parts(&mut acc, &queued(&full)));
        assert_eq!(acc.turn_anchor(), None, "the new message is still queued");
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(
            text.matches(tail).count(),
            1,
            "the orphan's tail renders exactly once: {text}"
        );
        assert!(
            !text.contains(prefix),
            "the delivered prefix is never repeated: {text}"
        );

        // The message lands and the new run answers: no duplication, and the
        // orphan's continuation reads before the new Turn's content.
        assert!(render_new_turn_parts(&mut acc, &answered(&full)));
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(text.matches(tail).count(), 1, "the tail stays once: {text}");
        assert!(text.contains("新回答"), "{text}");
        assert!(
            text.find(tail).unwrap() < text.find("新回答").unwrap(),
            "the orphan's continuation reads before the new answer: {text}"
        );
        assert!(!render_new_turn_parts(&mut acc, &answered(&full)));
    }

    /// A NEW Turn's answer identical to the orphan's delivered text still
    /// renders (spec #561, review #569): the seeded delivery suppresses the
    /// ORPHAN's own parts, never the new Turn's — the race requires the
    /// orphan's tail to read before the new content, and the new content must
    /// never be swallowed by the orphan's delivery.
    #[test]
    fn an_identical_new_answer_is_not_swallowed_by_the_seeds_delivery() {
        let new_anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: new_anchor - 60_000,
        };
        let answer = "同一个答案。";
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_new",
                MessageRole::User,
                Some(new_anchor),
                vec![text_part("新的问题")],
            ),
            // The orphan's answer, delivered in full by the cursor.
            message_in_flight(
                "a_orphan",
                new_anchor - 50_000,
                None,
                vec![text_at(answer, new_anchor - 50_000)],
            ),
            // The NEW Turn's own answer: the same text.
            message(
                "a_new",
                new_anchor + 1_000,
                vec![text_at(answer, new_anchor + 1_000)],
            ),
        ]);

        let mut acc = StreamAccumulator::new("proj");
        acc.set_cola_message_id("msg_cola_new");
        let cursor = projection_cursor_at(
            "a_orphan",
            0,
            CursorPartKind::Text,
            Some(new_anchor - 50_000),
            answer,
            &[],
        );
        let seed =
            CursorSeed::for_orphan_resolving(&transcript, &cursor, &orphan).expect("the cursor resolves");
        acc.seed_projection(&cursor, seed);

        let rendered = render_new_turn_parts(&mut acc, &transcript);
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(
            text.matches(answer).count(),
            1,
            "the new Turn's identical answer renders exactly once for the new turn: {text}"
        );
        assert!(rendered, "the new Turn's own answer is a render");
    }

    /// A frontier message inside the new Turn's in-flight window belongs to
    /// BOTH the seed's scope and the accumulator's own window: the scope walk
    /// renders its suffix and the window skips it, so one pass renders the tail
    /// exactly once (spec #561, ticket #565).
    #[test]
    fn a_message_first_seed_never_double_renders_a_straddling_message() {
        let new_anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: new_anchor - 60_000,
        };
        let prefix = "第一段。";
        let tail = "第二段。";
        let full = format!("{prefix}{tail}");
        // In flight, created before the new anchor but still recent: it
        // belongs to the orphan's Turn AND the new Turn's in-flight window.
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_new",
                MessageRole::User,
                Some(new_anchor),
                vec![text_part("新的问题")],
            ),
            message_in_flight(
                "a_orphan",
                new_anchor - 50_000,
                None,
                vec![text_at(&full, new_anchor - 50_000)],
            ),
        ]);

        let mut acc = StreamAccumulator::new("proj");
        acc.set_cola_message_id("msg_cola_new");
        let cursor = projection_cursor_at(
            "a_orphan",
            0,
            CursorPartKind::Text,
            Some(new_anchor - 50_000),
            prefix,
            &[],
        );
        let seed =
            CursorSeed::for_orphan_resolving(&transcript, &cursor, &orphan).expect("the cursor resolves");
        acc.seed_projection(&cursor, seed);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        let text = card_text(&acc.build_card_with_split().0);
        assert_eq!(
            text.matches(tail).count(),
            1,
            "the suffix renders exactly once across both windows: {text}"
        );
        assert!(
            !text.contains(prefix),
            "the delivered prefix is never repeated: {text}"
        );
        assert!(!render_new_turn_parts(&mut acc, &transcript));
    }

    /// The Rendered Cursor fixture of the projection tests (spec #561): a
    /// text/reasoning frontier at `(message, part)` with `delivered_chars`
    /// confirmed and the given live calls. The part carries no server clock,
    /// like the `text_part` fixtures — a cursor whose frontier names a part
    /// WITH a clock uses [`projection_cursor_at`].
    fn projection_cursor(
        message: &str,
        part_index: usize,
        kind: CursorPartKind,
        delivered: &str,
        live: &[&str],
    ) -> RenderedCursor {
        projection_cursor_at(message, part_index, kind, None, delivered, live)
    }

    /// [`projection_cursor`] with the part's server start time (ADR-0071's
    /// frontier identity, review #569).
    fn projection_cursor_at(
        message: &str,
        part_index: usize,
        kind: CursorPartKind,
        started_at: Option<i64>,
        delivered: &str,
        live: &[&str],
    ) -> RenderedCursor {
        RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new(message),
                part_index,
                kind,
                started_at,
                delivered_chars: delivered.chars().count(),
                prefix_digest: Some(cursor_prefix_digest(delivered)),
            }),
            live_calls: live.iter().map(|id| id.to_string()).collect(),
        }
    }

    /// The ended-while-down projection's seed (spec #561, ticket #563):
    /// everything at or before the frontier counts as delivered — it is not
    /// rendered again — and the newest part renders only its undelivered
    /// suffix.
    #[test]
    fn a_seeded_render_continues_a_cut_text_part_at_the_cursor() {
        let prefix = "第一段回答。";
        let suffix = "第二段回答。";
        let full = format!("{prefix}{suffix}");
        let transcript = |text: &str| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message("msg_a_2000", 2_000, vec![text_part(text)]),
            ])
        };

        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, prefix, &[]);
        let seed = CursorSeed::resolve(&transcript(&full), &cursor).expect("the frontier resolves");
        acc.seed_projection(&cursor, seed);

        assert!(render_new_turn_parts(&mut acc, &transcript(&full)));
        let built = acc.build_card_with_info();
        let text = card_text(&built.card);
        assert!(
            !text.contains(prefix),
            "the delivered prefix is not rendered again: {text}"
        );
        assert!(text.contains(suffix), "the undelivered suffix renders: {text}");
        assert_eq!(
            built.cursor.frontier,
            Some(CursorFrontier {
                message_id: MessageId::new("msg_a_2000"),
                part_index: 0,
                kind: CursorPartKind::Text,
                // The fixture's part carries no server clock; the cursor
                // keeps the part's position either way.
                started_at: None,
                delivered_chars: full.chars().count(),
                prefix_digest: Some(cursor_prefix_digest(&full)),
            }),
            "the body's cursor continues from the cut, not from zero"
        );

        // An unchanged read renders nothing new (the Wake content-diff must
        // not replay the delivered prefix); growth renders only the new tail.
        assert!(!render_new_turn_parts(&mut acc, &transcript(&full)));
        let grown = format!("{full}第三段回答。");
        assert!(render_new_turn_parts(&mut acc, &transcript(&grown)));
        let text = card_text(&acc.build_card_with_info().card);
        assert!(text.contains("第三段回答。"), "{text}");
        assert_eq!(
            text.matches(suffix).count(),
            1,
            "growth never duplicates the delivered suffix: {text}"
        );
    }

    /// The ordinary render's cursor arithmetic (spec #561, Codex review on PR
    /// #569): the server grows an in-flight part by re-sending it whole, so
    /// the render must push only the undelivered delta — the same part's
    /// entries stay disjoint chunks and the frontier's extent is the part's
    /// own length, never a sum over overlapping snapshots. Before the fix the
    /// "AB" read re-pushed the whole snapshot ("AAB") and the frontier read 3.
    #[test]
    fn a_grown_part_renders_only_its_new_tail_and_counts_the_part_once() {
        let transcript = |text: &str| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message("msg_a_2000", 2_000, vec![text_at(text, 2_000)]),
            ])
        };
        let mut acc = StreamAccumulator::new("t");
        acc.repoint_turn_anchor(&turn_anchor(1_000));

        assert!(render_new_turn_parts(&mut acc, &transcript("A")));
        let built = acc.build_card_with_info();
        let text = card_text(&built.card);
        assert_eq!(
            text.matches('A').count(),
            1,
            "the first snapshot renders once: {text}"
        );
        assert_eq!(
            built.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(1),
            "the frontier counts the delivered part"
        );

        // The server grew the same part: only the new tail is appended.
        assert!(render_new_turn_parts(&mut acc, &transcript("AB")));
        let built = acc.build_card_with_info();
        let text = card_text(&built.card);
        assert!(
            !text.contains("AAB") && text.matches('A').count() == 1,
            "the grown snapshot never repeats the delivered prefix: {text}"
        );
        assert_eq!(
            built.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(2),
            "the frontier is the part's character extent, not a sum over its snapshots"
        );

        // And again: the delta rule holds for every growth.
        assert!(render_new_turn_parts(&mut acc, &transcript("ABC")));
        let built = acc.build_card_with_info();
        let text = card_text(&built.card);
        assert!(
            !text.contains("AAB") && text.matches('A').count() == 1,
            "the further growth never repeats the prefix: {text}"
        );
        assert_eq!(
            built.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(3),
            "the frontier keeps counting the whole part exactly once"
        );
        // An unchanged read renders nothing new.
        assert!(!render_new_turn_parts(&mut acc, &transcript("ABC")));
    }

    /// The delta rule at the card-size boundary (spec #561, Codex review on PR
    /// #569): a part whose chunk already fills its timeline entry grows into a
    /// new entry, and the frontier still counts the part exactly once — the
    /// growth must not re-push the chunk the earlier entry delivered.
    #[test]
    fn a_grown_split_part_keeps_the_cursor_extent_exact() {
        let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
        let first: String = "长".repeat(max);
        let transcript = |text: &str| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message("msg_a_2000", 2_000, vec![text_at(text, 2_000)]),
            ])
        };
        let mut acc = StreamAccumulator::new("t");
        acc.repoint_turn_anchor(&turn_anchor(1_000));

        assert!(render_new_turn_parts(&mut acc, &transcript(&first)));
        let delivered = acc.build_card_with_info();
        assert!(!delivered.full, "the part exactly fills one card");
        assert_eq!(
            delivered.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(max),
            "the single chunk is delivered whole"
        );

        // The part grows past the entry's chunk: the tail opens a new entry.
        let tail = "尾巴。";
        let grown = format!("{first}{tail}");
        assert!(render_new_turn_parts(&mut acc, &transcript(&grown)));
        let finalized = acc.build_card_with_info();
        assert!(finalized.full, "the over-budget part finalizes the first card");
        assert_eq!(
            finalized.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(max),
            "the finalized slice delivered the first chunk"
        );
        let continuation = acc.build_card_with_info();
        assert_eq!(
            continuation.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(max + tail.chars().count()),
            "the continuation counts the part once across both entries"
        );
        let text = card_text(&continuation.card);
        assert_eq!(text.matches(tail).count(), 1, "the grown tail lands once: {text}");
    }

    /// A tail cut inside a code fence (spec #561, ticket #563): the seeded
    /// suffix renders intact — the fence reopens, so the code stays code and
    /// the text after the original closer is not swallowed.
    #[test]
    fn a_seeded_tail_cut_inside_a_fence_renders_intact() {
        let prefix = "说明\n```python\nprint(1)\n";
        let full = format!("{prefix}print(2)\n```\n后的文字");
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_anchor",
                MessageRole::User,
                Some(1_000),
                vec![text_part("问题")],
            ),
            message("msg_a_2000", 2_000, vec![text_part(&full)]),
        ]);

        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, prefix, &[]);
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the frontier resolves");
        acc.seed_projection(&cursor, seed);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        let text = card_text(&acc.build_card_with_info().card);
        assert!(
            text.contains("```\nprint(2)\n```\n后的文字"),
            "the cut tail reopens the fence and keeps the after-text intact: {text}"
        );
        assert!(
            !text.contains(prefix),
            "the delivered fence prefix is not repeated: {text}"
        );
    }

    /// A reasoning frontier renders only its undelivered suffix too; a part
    /// after the frontier renders in full (spec #561, ticket #563).
    #[test]
    fn a_seeded_reasoning_part_renders_its_tail_and_later_parts_in_full() {
        let full_reasoning = "先想第一步。再想第二步。";
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_anchor",
                MessageRole::User,
                Some(1_000),
                vec![text_part("问题")],
            ),
            message(
                "msg_a_2000",
                2_000,
                vec![reasoning_at(full_reasoning, 2_000), text_part("表后的答案。")],
            ),
        ]);

        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        // The reasoning part delivered up to its first sentence.
        let cursor = projection_cursor_at(
            "msg_a_2000",
            0,
            CursorPartKind::Reasoning,
            Some(2_000),
            "先想第一步。",
            &[],
        );
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the frontier resolves");
        acc.seed_projection(&cursor, seed);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        let text = card_text(&acc.build_card_with_info().card);
        assert!(
            text.contains("再想第二步。"),
            "the reasoning suffix renders: {text}"
        );
        assert!(
            !text.contains("先想第一步。"),
            "the delivered reasoning is not repeated: {text}"
        );
        assert!(
            text.contains("表后的答案。"),
            "a part after the frontier renders in full: {text}"
        );
    }

    /// A reasoning part longer than the card's reasoning cap (spec #561,
    /// review #569): the card shows only the first 800 characters, so the
    /// frontier must record only those — the undisclosed suffix is NOT
    /// delivered. Before the fix the frontier counted the whole part, the
    /// read-side resolution verified it, and the undisclosed content never
    /// rendered.
    #[test]
    fn a_long_reasoning_part_confirms_only_what_the_card_shows() {
        let shown = "【已显示的前缀】";
        let hidden = "【未显示的后缀】";
        let full = format!("{shown}{}{hidden}", "隐".repeat(800));
        assert!(full.chars().count() > 800);
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_anchor",
                MessageRole::User,
                Some(1_000),
                vec![text_part("问题")],
            ),
            message("msg_a_2000", 2_000, vec![reasoning_at(&full, 2_000)]),
        ]);

        // The live render: the whole part is pushed, the card truncates it.
        let mut acc = StreamAccumulator::new("live");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        assert!(render_new_turn_parts(&mut acc, &transcript));
        let built = acc.build_card_with_info();
        let card = card_text(&built.card);
        assert!(
            card.contains(shown) && card.contains('…'),
            "the card shows the capped reasoning with its ellipsis: {card}"
        );
        let delivered = built
            .cursor
            .frontier
            .as_ref()
            .expect("the reasoning frontier")
            .delivered_chars;
        assert_eq!(
            delivered,
            crate::feishu::card::REASONING_TEXT_CAP,
            "the frontier counts only the characters the card displayed"
        );

        // The restart: the same read, a fresh accumulator, the seed from that
        // frontier. The undisclosed suffix renders as a continuation.
        let mut restarted = StreamAccumulator::new("proj");
        restarted.repoint_turn_anchor(&turn_anchor(1_000));
        let seed = CursorSeed::resolve(&transcript, &built.cursor).expect("the frontier resolves");
        restarted.seed_projection(&built.cursor, seed);
        assert!(render_new_turn_parts(&mut restarted, &transcript));
        let recovered = card_text(&restarted.build_card_with_info().card);
        assert!(
            recovered.contains(hidden),
            "the content beyond the cap renders on recovery: {recovered}"
        );
        assert!(
            !recovered.contains(shown),
            "the delivered prefix is not repeated: {recovered}"
        );
    }

    /// A reasoning part that grows from BELOW the cap to above it (spec #561,
    /// review #569): the incremental push merges into the part's own entry, so
    /// its digest must fingerprint exactly the delivered prefix the card shows
    /// — `min(part length, cap)` — never `delivered + cap`. Before the fix the
    /// digest covered the whole grown part, the clamped extent could not verify
    /// it, and the recovery re-rendered the part from zero.
    #[test]
    fn a_reasoning_part_growing_across_the_cap_keeps_its_digest_in_step() {
        let shown = "【已显示的】";
        let beyond = "【跨过上限后才有的】";
        let first = format!("{shown}{}", "隐".repeat(700));
        // The growth crosses the cap: the marker sits past it, where only the
        // recovery may show it.
        let grown = format!("{first}{}{beyond}", "跨".repeat(200));
        assert!(first.chars().count() <= crate::feishu::card::REASONING_TEXT_CAP);
        assert!(grown.chars().count() > crate::feishu::card::REASONING_TEXT_CAP);
        let build = |reasoning: &str| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message("msg_a_2000", 2_000, vec![reasoning_at(reasoning, 2_000)]),
            ])
        };

        // The live render: the first push is below the cap, the growth merges
        // into the same entry and crosses it.
        let mut acc = StreamAccumulator::new("live");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        assert!(render_new_turn_parts(&mut acc, &build(&first)));
        assert!(render_new_turn_parts(&mut acc, &build(&grown)));
        let built = acc.build_card_with_info();
        let delivered = built
            .cursor
            .frontier
            .as_ref()
            .expect("the reasoning frontier")
            .delivered_chars;
        assert_eq!(
            delivered,
            crate::feishu::card::REASONING_TEXT_CAP,
            "the grown frontier is the cap, not the part's full length"
        );

        // The restart: the frontier RESOLVES (its digest must hash the read's
        // capped prefix) and renders only what is beyond it.
        let restarted_read = build(&grown);
        let mut recovered = StreamAccumulator::new("proj");
        recovered.repoint_turn_anchor(&turn_anchor(1_000));
        let seed = CursorSeed::resolve(&restarted_read, &built.cursor).expect("the grown frontier resolves");
        recovered.seed_projection(&built.cursor, seed);
        assert!(render_new_turn_parts(&mut recovered, &restarted_read));
        let text = card_text(&recovered.build_card_with_info().card);
        assert!(
            text.contains(beyond),
            "the content beyond the cap renders on recovery: {text}"
        );
        assert!(
            !text.contains(shown),
            "nothing the card displayed repeats: {text}"
        );
    }

    /// A long reasoning part that grew while cola was down (spec #561, review
    /// #569): the recovery renders everything beyond the delivered extent —
    /// the undisclosed middle AND the growth — never repeating what a card
    /// showed.
    #[test]
    fn a_grown_long_reasoning_part_renders_only_what_is_beyond_the_extent() {
        let shown = "【已显示的】";
        let middle = "【未显示的中间】";
        let growth = "【停机期间的追加】";
        let before = format!("{shown}{}{middle}", "隐".repeat(800));
        let after = format!("{before}{growth}");

        let build = |reasoning: &str| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message("msg_a_2000", 2_000, vec![reasoning_at(reasoning, 2_000)]),
            ])
        };
        let mut acc = StreamAccumulator::new("live");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        assert!(render_new_turn_parts(&mut acc, &build(&before)));
        let cursor = acc.build_card_with_info().cursor;

        let restarted = build(&after);
        let mut recovered = StreamAccumulator::new("proj");
        recovered.repoint_turn_anchor(&turn_anchor(1_000));
        let seed = CursorSeed::resolve(&restarted, &cursor).expect("the frontier resolves");
        recovered.seed_projection(&cursor, seed);
        assert!(render_new_turn_parts(&mut recovered, &restarted));
        let text = card_text(&recovered.build_card_with_info().card);
        assert!(
            text.contains(middle) && text.contains(growth),
            "the undisclosed middle and the growth render: {text}"
        );
        assert!(
            !text.contains(shown),
            "nothing at or before the delivered extent repeats: {text}"
        );
    }

    /// A tail cut inside a markdown table repeats the table's header and
    /// delimiter, so the remaining rows parse as the table they continue
    /// (spec #561, ticket #563).
    #[test]
    fn a_seeded_tail_cut_inside_a_table_repeats_its_header() {
        let prefix = "| 名称 | 值 |\n|---|---|\n| 一 | 1 |\n";
        let full = format!("{prefix}| 二 | 2 |\n\n尾注");
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_anchor",
                MessageRole::User,
                Some(1_000),
                vec![text_part("问题")],
            ),
            message("msg_a_2000", 2_000, vec![text_part(&full)]),
        ]);

        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, prefix, &[]);
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the frontier resolves");
        acc.seed_projection(&cursor, seed);

        assert!(render_new_turn_parts(&mut acc, &transcript));
        let text = card_text(&acc.build_card_with_info().card);
        assert!(
            text.contains("| 名称 | 值 |\n|---|---|\n| 二 | 2 |"),
            "the table cut repeats its header: {text}"
        );
        assert!(
            !text.contains("| 一 | 1 |"),
            "the delivered rows are not repeated: {text}"
        );
        assert!(text.contains("尾注"), "{text}");
    }

    /// A tool the delivered body showed running (the cursor's live set) and
    /// which settled while cola was down renders its result on the successor
    /// exactly once (spec #561, ticket #563).
    #[test]
    fn a_seeded_render_settles_a_live_set_call_exactly_once() {
        let text = "第一段回答。";
        let timeline = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message("msg_a_2000", 2_000, vec![text_part(text)]),
                message(
                    "msg_tool_3000",
                    3_000,
                    vec![tool(
                        "bash",
                        "call_1",
                        status,
                        Some(3_000),
                        Some(serde_json::json!({ "command": "sleep 300" })),
                        output,
                    )],
                ),
            ])
        };
        let running = timeline(ToolStatus::Running, None);
        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, text, &["call_1"]);
        let seed = CursorSeed::resolve(&running, &cursor).expect("the frontier resolves");
        acc.seed_projection(&cursor, seed);

        // The delivered body's newest state for the call was `running`.
        assert!(render_new_turn_parts(&mut acc, &running));
        assert!(
            card_text(&acc.build_card_with_info().card).contains("⏳ bash"),
            "the delivered running panel is still live on the successor"
        );

        // Settled while cola was down: the result joins the timeline exactly
        // once and the `⏳` is gone.
        let settled = timeline(ToolStatus::Completed, Some("done"));
        assert!(render_new_turn_parts(&mut acc, &settled));
        assert!(
            acc.seeded_calls().is_empty(),
            "a settled live-set call leaves the display-only carry"
        );
        let text_after = card_text(&acc.build_card_with_info().card);
        assert!(text_after.contains("done"), "{text_after}");
        assert_eq!(
            text_after.matches("done").count(),
            1,
            "settled once: {text_after}"
        );
        assert!(
            !text_after.contains("⏳"),
            "no frozen running marker: {text_after}"
        );
        assert!(!render_new_turn_parts(&mut acc, &settled));
        let text_after = card_text(&acc.build_card_with_info().card);
        assert_eq!(
            text_after.matches("done").count(),
            1,
            "no duplication: {text_after}"
        );
    }

    /// A live-set call delivered running at or before the frontier stays in
    /// the identity carry after the seeded render, so its later settlement
    /// still joins the timeline exactly once (spec #561, ticket #563).
    #[test]
    fn a_seeded_live_set_call_before_the_frontier_still_settles_once() {
        let text = "第一段回答。";
        let timeline = |status: ToolStatus, output: Option<&str>| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message(
                    "msg_tool_1500",
                    1_500,
                    vec![tool(
                        "bash",
                        "call_1",
                        status,
                        Some(1_500),
                        Some(serde_json::json!({ "command": "sleep 300" })),
                        output,
                    )],
                ),
                message("msg_a_2000", 2_000, vec![text_part(text)]),
            ])
        };
        let running = timeline(ToolStatus::Running, None);
        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, text, &["call_1"]);
        let seed = CursorSeed::resolve(&running, &cursor).expect("the frontier resolves");
        acc.seed_projection(&cursor, seed);

        assert!(render_new_turn_parts(&mut acc, &running));
        assert!(
            acc.seeded_calls().contains("call_1"),
            "the delivered running call stays in the identity carry even before the frontier"
        );

        // The call settles: the carry still resolves it, exactly once.
        let settled = timeline(ToolStatus::Completed, Some("done"));
        assert!(render_new_turn_parts(&mut acc, &settled));
        assert!(acc.seeded_calls().is_empty(), "the settled call leaves the carry");
        let text = card_text(&acc.build_card_with_info().card);
        assert_eq!(text.matches("done").count(), 1, "settled once: {text}");
        assert!(!render_new_turn_parts(&mut acc, &settled));
        let text = card_text(&acc.build_card_with_info().card);
        assert_eq!(text.matches("done").count(), 1, "no duplication: {text}");
    }

    /// A tool frontier whose recorded start time no longer matches the read is
    /// a REPLACEMENT (spec #561, review #569): with no text before it, nothing
    /// may be skipped past it — the new tool renders its result instead of
    /// being marked delivered.
    #[test]
    fn a_replaced_settled_tool_frontier_renders_its_result() {
        let transcript = SessionTranscript::new(vec![message(
            "msg_a_1",
            100,
            vec![tool(
                "bash",
                "call_new",
                ToolStatus::Completed,
                Some(200),
                Some(serde_json::json!({ "command": "echo new" })),
                Some("new-result"),
            )],
        )]);
        let mut acc = StreamAccumulator::new("t");
        acc.repoint_turn_anchor(&turn_anchor(50));
        // The cursor named a settled tool in this slot, with no text before it.
        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 0,
                kind: CursorPartKind::Tool,
                started_at: Some(150),
                delivered_chars: 0,
                prefix_digest: Some(cursor_prefix_digest("")),
            }),
            live_calls: Default::default(),
        };
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the slot resolves");
        acc.seed_projection(&cursor, seed);

        render_new_turn_parts(&mut acc, &transcript);
        let text = card_text(&acc.build_card_with_info().card);
        assert!(
            text.contains("new-result"),
            "the replacement tool's result renders: {text}"
        );
    }

    /// Everything at or before the frontier is delivered — including a tool
    /// whose result the old card already showed — so the successor never
    /// renders it again (spec #561, ticket #563).
    #[test]
    fn a_seeded_render_skips_a_settled_tool_before_the_frontier() {
        let prefix = "第一段回答。";
        let full = format!("{prefix}第二段回答。");
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_anchor",
                MessageRole::User,
                Some(1_000),
                vec![text_part("问题")],
            ),
            message(
                "msg_tool_1500",
                1_500,
                vec![tool(
                    "bash",
                    "call_old",
                    ToolStatus::Completed,
                    Some(1_500),
                    Some(serde_json::json!({ "command": "echo old" })),
                    Some("old-result"),
                )],
            ),
            message("msg_a_2000", 2_000, vec![text_part(&full)]),
        ]);

        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, prefix, &[]);
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the frontier resolves");
        acc.seed_projection(&cursor, seed);

        render_new_turn_parts(&mut acc, &transcript);
        let text = card_text(&acc.build_card_with_info().card);
        assert!(text.contains("第二段回答。"), "{text}");
        assert!(
            !text.contains("old-result"),
            "a delivered settled tool is never rendered again: {text}"
        );
    }

    /// The header date reads the SERVER's time for the turn's user message
    /// (#183 follow-up): captured on the first poll that sees it, so cola's
    /// own clock never reaches the card. The same captured anchor is #190's
    /// turn filter, so this test also pins the capture source.
    #[test]
    fn turn_anchor_captures_the_user_message_identity_and_server_time() {
        let mut acc = StreamAccumulator::new("proj");
        acc.set_cola_message_id("msg_cola_1");
        let transcript = SessionTranscript::new(vec![typed_message(
            "msg_cola_1",
            MessageRole::User,
            Some(1234),
            vec![text_part("你好")],
        )]);

        assert!(!render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(
            acc.turn_anchor(),
            Some(&TurnAnchor {
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
        acc.set_cola_message_id("msg_cola_1");
        let transcript = SessionTranscript::new(vec![message("a1", 100, vec![text_part("回答")])]);

        assert!(!render_new_turn_parts(&mut acc, &transcript));
        assert_eq!(acc.turn_anchor(), None);
        assert!(acc.text().is_empty());
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
        acc.set_cola_message_id("msg_cola_1");
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
            acc.turn_anchor(),
            Some(&TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: server_user,
            })
        );
        assert!(
            acc.text().contains("回答"),
            "the turn's parts must render: {:?}",
            acc.text()
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
        acc.set_cola_message_id("msg_cola_1");
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
            acc.turn_anchor(),
            Some(&TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: server_user,
            })
        );
        assert!(
            acc.text().contains("新回答"),
            "the turn must render: {:?}",
            acc.text()
        );
        assert!(
            !acc.text().contains("旧回答"),
            "the previous turn must not bleed in: {:?}",
            acc.text()
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
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        assert_eq!(acc.tools()["call_1"].status(), &ToolStatus::Running);

        // Same call, updated to completed — must re-render (upsert).
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(ToolStatus::Completed, "src\n")
        ));
        assert_eq!(acc.tools()["call_1"].status(), &ToolStatus::Completed);

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
            acc.tools().is_empty(),
            "todowrite must not take a timeline row: {:?}",
            acc.tools().keys().collect::<Vec<_>>()
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
        acc.set_card_state(CardState::Done);
        // A near-maximal todo panel: one item whose CJK content fills the
        // reserve's 3000-char output window (~9KB).
        let output = serde_json::json!([{
            "content": "很长的任务描述".repeat(500),
            "status": "pending",
        }])
        .to_string();
        acc.set_todo_panel(
            crate::feishu::card::tool_render::ToolPanel::for_test(
                "todowrite",
                ToolStatus::Completed,
                None,
                Some(&output),
            ),
            None,
        );
        // Two max-size CJK text chunks (~18KB each): the first alone plus the
        // reserve is over the budget, so only the progress guarantee keeps the
        // chain moving.
        acc.push_text(&"很长的回答。".repeat(1000));
        acc.push_text(&"另一段回答。".repeat(1000));

        let mut cards = 0;
        loop {
            let before = acc.render_from();
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
            assert!(acc.render_from() > before, "a full card must advance render_from");
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
        assert!(acc.todo_panel().is_some(), "the tail holds the panel");

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
    /// thing it renders — a reasoning panel from its timeline item, a tool
    /// panel from its call (`tool_{call_id}`) — never from its position. The
    /// card is re-rendered whole on every flush while the client holds each
    /// panel's open/closed state locally; an id that moved would hand that
    /// state to a different panel, which is how the todo panel used to lose its
    /// expansion.
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
        assert!(
            before[1].starts_with("tool_call_1("),
            "a tool panel's id names its call: {before:?}"
        );
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
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        assert_eq!(acc.reasoning(), "");
        assert_eq!(acc.text(), "");

        // Once content lands (same message/parts), render it once.
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript("Let me think", "Answer here")
        ));
        assert!(acc.reasoning().contains("Let me think"));
        assert!(acc.text().contains("Answer here"));

        // Re-fetching the same content must not duplicate.
        assert!(!render_new_turn_parts(
            &mut acc,
            &transcript("Let me think", "Answer here")
        ));
        assert_eq!(acc.reasoning(), "Let me think");
        assert_eq!(acc.text(), "Answer here");
    }

    /// Regression: real OpenCode part payloads carry NO `id` field (the DB id
    /// is not serialised into the part JSON). Dedup must fall back to content,
    /// otherwise the poll loop + final render append the same text twice
    /// (observed: card text 81 → 162 chars).
    #[test]
    fn text_without_id_is_not_rendered_twice() {
        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        assert_eq!(acc.text(), "你好！很高兴认识你。");

        // Final render re-fetches the same messages — must NOT append again.
        assert!(!render_new_turn_parts(&mut acc, &transcript()));
        assert_eq!(acc.text(), "你好！很高兴认识你。");
        assert_eq!(acc.reasoning(), "thinking");
    }

    /// Real OpenCode failed-tool calls use status `error` with the reason in
    /// the output's error side and no output text — the panel must show the
    /// error and mark the card failed, not stay stuck "running".
    #[test]
    fn failed_tool_error_part_renders_output_and_error_state() {
        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        let tool = acc.tools().get("call_edit").expect("tool rendered");
        assert_eq!(tool.status(), &ToolStatus::Error);
        let out = tool.output().unwrap_or_default();
        assert!(
            out.contains("no such file"),
            "error message must be shown: {}",
            out
        );

        // The Done card header stays green — the failure is on the tool's own
        // panel, not the whole turn.
        acc.set_card_state(crate::feishu::card::CardState::Done);
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
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        let tool = acc.tools().get("call_edit").expect("tool rendered");
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
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        let out = acc.tools()["call_edit"].output().unwrap_or_default();
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
        acc.repoint_turn_anchor(&turn_anchor(0));
        assert!(render_new_turn_parts(
            &mut acc,
            &transcript(edit(ToolStatus::Error, None, true, Some("no such file")))
        ));
        let out = acc.tools()["call_edit"].output().unwrap_or_default();
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
        acc.repoint_turn_anchor(&turn_anchor(0));

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
        assert_eq!(acc.text(), "The answer.");
        assert_eq!(acc.reasoning(), "Let me check");
        assert_eq!(acc.tools()["call_1"].output().as_deref(), Some("src"));
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

    /// A `read` tool call whose decoded output carries the text header plus one
    /// File Content (ADR-0076), the shape `read` returns for an image.
    fn read_file_part(call_id: &str, name: &str, mime: &str, bytes: &[u8]) -> Part {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let content = crate::backend::FileContent::decode(
            &format!("data:{mime};base64,{encoded}"),
            Some(mime),
            Some(name),
        )
        .expect("an inline payload is a File Content");
        Part::Tool(tool_call(
            "read",
            call_id,
            ToolStatus::Completed,
            Some(1_000),
            None,
            vec![
                crate::backend::ContentBlock::Text("Image read successfully".into()),
                crate::backend::ContentBlock::File(content),
            ],
        ))
    }

    /// [`read_file_part`] with the File Content's decoded `size` overridden — the
    /// shape of a payload past Feishu's caps, which a test cannot build as real
    /// bytes without allocating 30MB.
    fn read_file_part_sized(call_id: &str, name: &str, mime: &str, bytes: &[u8], size: u64) -> Part {
        let Part::Tool(mut call) = read_file_part(call_id, name, mime, bytes) else {
            unreachable!("read_file_part builds a tool part");
        };
        for block in &mut call.output.blocks {
            if let crate::backend::ContentBlock::File(content) = block {
                content.size = size;
            }
        }
        Part::Tool(call)
    }

    /// Drive one render pass for `sid` with `transcript` and return it.
    async fn render(
        app: &Arc<App>,
        cards: &CardsHandle,
        sid: &str,
        transcript: &SessionTranscript,
    ) -> Option<RenderPass> {
        render_and_flush(
            cards,
            &app.sessions_handle(),
            &app.opencode,
            &app.requests_handle(),
            sid,
            transcript,
        )
        .await
    }

    /// The `(mime, size)` of every image upload the mock recorded, in order.
    async fn uploaded_images(platform: &RecordingPlatform) -> Vec<(String, u64)> {
        platform
            .calls
            .lock()
            .await
            .iter()
            .filter_map(|call| match call {
                PlatformCall::UploadImage { mime, size } => Some((mime.clone(), *size)),
                _ => None,
            })
            .collect()
    }

    /// The `(name, size)` of every file upload the mock recorded, in order.
    async fn uploaded_files(platform: &RecordingPlatform) -> Vec<(String, u64)> {
        platform
            .calls
            .lock()
            .await
            .iter()
            .filter_map(|call| match call {
                PlatformCall::UploadFile { name, size } => Some((name.clone(), *size)),
                _ => None,
            })
            .collect()
    }

    /// Every in-thread message the mock recorded, in order:
    /// `(target message id, msg_type, content)`.
    async fn sent_messages(platform: &RecordingPlatform) -> Vec<(String, String, serde_json::Value)> {
        platform
            .calls
            .lock()
            .await
            .iter()
            .filter_map(|call| match call {
                PlatformCall::SendMessageInThread {
                    message_id,
                    msg_type,
                    content,
                } => Some((message_id.clone(), msg_type.clone(), content.clone())),
                _ => None,
            })
            .collect()
    }

    /// A tool-read image renders on the card (ADR-0076): uploaded once, embedded
    /// as an `img` immediately after its panel with `title` `📎 <name>` and
    /// `preview` on, and the panel carries the `· 已内嵌` tracking line.
    #[tokio::test]
    async fn a_tool_read_image_is_uploaded_and_embedded_after_its_panel() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        platform.given_image_key("img_v2_shot");
        let sid = "ses_embed_image";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_embed")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let pass = render(
            &app,
            &cards,
            sid,
            &SessionTranscript::new(vec![message(
                "a1",
                1_000,
                vec![read_file_part("call_read", "shot.png", "image/png", b"ABC")],
            )]),
        )
        .await;
        assert!(pass.is_some(), "the render pass must not vanish");

        assert_eq!(
            uploaded_images(&platform).await,
            vec![("image/png".to_string(), 3)],
            "exactly one upload, with the decoded size"
        );
        let card = final_card(&platform).await;
        let elements = card["body"]["elements"].as_array().unwrap();
        let panel_at = elements
            .iter()
            .position(|e| e["tag"] == "collapsible_panel")
            .expect("the read panel is on the card");
        let image = &elements[panel_at + 1];
        assert_eq!(image["tag"], "img", "the image follows the panel: {card}");
        assert_eq!(image["img_key"], "img_v2_shot");
        assert_eq!(image["title"]["content"], "📎 shot.png");
        assert_eq!(image["preview"], true);
        let body = elements[panel_at]["elements"][0]["content"].as_str().unwrap();
        assert!(body.contains("已内嵌"), "the panel marks the embed: {body}");
        assert!(body.contains("📎 shot.png · image/png · 3 B"), "{body}");
    }

    /// The same bytes read twice in one Session upload once: the second render
    /// resolves the content from the process-local cache and every later PATCH
    /// reuses the key (ADR-0076).
    #[tokio::test]
    async fn reading_the_same_image_twice_uploads_once_and_reuses_the_key() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        platform.given_image_key("img_v2_same");
        let sid = "ses_embed_dedup";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_dedup")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let first = SessionTranscript::new(vec![message(
            "a1",
            1_000,
            vec![read_file_part("call_read", "shot.png", "image/png", b"ABC")],
        )]);
        render(&app, &cards, sid, &first).await;

        // A second call reads the SAME bytes into the same Session.
        let second = SessionTranscript::new(vec![
            message(
                "a1",
                1_000,
                vec![read_file_part("call_read", "shot.png", "image/png", b"ABC")],
            ),
            message(
                "a2",
                2_000,
                vec![read_file_part("call_read_2", "shot.png", "image/png", b"ABC")],
            ),
        ]);
        render(&app, &cards, sid, &second).await;

        assert_eq!(
            uploaded_images(&platform).await.len(),
            1,
            "identical bytes upload once"
        );
        assert_eq!(
            platform
                .upload_image_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second read is a cache hit, not a second platform call"
        );
        let card = final_card(&platform).await;
        let images: Vec<_> = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "img")
            .collect();
        assert_eq!(images.len(), 2, "both reads show their image: {card}");
        assert!(images.iter().all(|img| img["img_key"] == "img_v2_same"));
    }

    /// Two resolves of the SAME bytes racing on one content hash await ONE
    /// upload (ADR-0076): the cache's in-flight guard is single-flight, so a
    /// resolver that arrives while the first is still uploading blocks on the
    /// shared cell instead of missing the cache and uploading the bytes again.
    #[tokio::test]
    async fn concurrent_resolves_of_the_same_bytes_upload_once() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        platform.given_image_key("img_v2_single_flight");
        let sid = "ses_embed_single_flight";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_single_flight")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let transcript = SessionTranscript::new(vec![message(
            "a1",
            1_000,
            vec![read_file_part("call_read", "shot.png", "image/png", b"ABC")],
        )]);

        // Park the first upload so the render's resolve is still in flight when
        // a second resolve of the same bytes arrives.
        let (entered, release) = platform.pause("upload_image", "");
        let first = {
            let app = Arc::clone(&app);
            let sid = sid.to_string();
            let transcript = transcript.clone();
            tokio::spawn(async move {
                let cards = app.core.cards_handle();
                render(&app, &cards, &sid, &transcript).await;
            })
        };
        entered.notified().await;

        // The second resolver races the parked upload for the SAME content hash.
        let second = {
            let app = Arc::clone(&app);
            let sid = sid.to_string();
            tokio::spawn(async move {
                let cards = app.core.cards_handle();
                resolve_file_deliveries(&cards, &sid).await
            })
        };
        // Let the second task run: single-flight leaves it blocked on the
        // in-flight cell (not finished), having made no second upload call.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !second.is_finished(),
            "the second resolver must await the in-flight upload, not finish"
        );
        assert_eq!(
            platform
                .upload_image_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second resolver must not start a second upload"
        );

        release.notify_one();
        first.await.unwrap();
        second.await.unwrap();

        assert_eq!(
            platform
                .upload_image_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one upload for two concurrent resolves of identical bytes"
        );
        assert_eq!(
            uploaded_images(&platform).await.len(),
            1,
            "exactly one UploadImage platform call"
        );
    }

    /// A File Content that is not an embeddable image (a PDF) is uploaded once
    /// and posted as exactly ONE File Message replied in-thread under the live
    /// card (#649); the panel line reads `已发送为文件消息` and no `img` is
    /// emitted. A later poll re-renders the line but never repeats the send.
    #[tokio::test]
    async fn a_non_image_file_content_is_sent_as_one_file_message() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        platform.given_file_key("file_v2_pdf");
        let sid = "ses_file_pdf";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_pdf")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let transcript = SessionTranscript::new(vec![message(
            "a1",
            1_000,
            vec![read_file_part("call_read", "doc.pdf", "application/pdf", b"%PDF")],
        )]);
        render(&app, &cards, sid, &transcript).await;

        assert!(
            uploaded_images(&platform).await.is_empty(),
            "a PDF is not an embeddable image"
        );
        assert_eq!(
            uploaded_files(&platform).await,
            vec![("doc.pdf".to_string(), 4)],
            "the file is uploaded once, with its decoded size"
        );
        assert_eq!(
            sent_messages(&platform).await,
            vec![(
                "om_pdf".to_string(),
                "file".to_string(),
                serde_json::json!({ "file_key": "file_v2_pdf" }),
            )],
            "exactly one File Message, replied in-thread under the live card"
        );
        let card = final_card(&platform).await;
        assert!(
            card["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["tag"] != "img"),
            "no image element: {card}"
        );
        let body = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(
            body.contains("📎 doc.pdf · application/pdf · 4 B · 已发送为文件消息"),
            "{body}"
        );

        // A later poll renders new content (forcing its PATCH) but never repeats
        // the File Message: the send is once, on the first poll the block
        // appeared, while the tracking line keeps saying it was sent.
        let second = SessionTranscript::new(vec![
            message(
                "a1",
                1_000,
                vec![read_file_part("call_read", "doc.pdf", "application/pdf", b"%PDF")],
            ),
            message("a2", 2_000, vec![text_part("done")]),
        ]);
        render(&app, &cards, sid, &second).await;
        assert_eq!(
            sent_messages(&platform).await.len(),
            1,
            "the File Message is sent on the first poll only"
        );
        assert_eq!(
            uploaded_files(&platform).await.len(),
            1,
            "the bytes are uploaded once"
        );
        let body = final_card(&platform).await["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            body.contains("已发送为文件消息"),
            "the later render keeps the line: {body}"
        );
    }

    /// A File Content past Feishu's 30MB message cap is never uploaded and never
    /// sent: it renders only `未发送` (#649).
    #[tokio::test]
    async fn an_oversize_file_content_is_never_uploaded_and_reads_undelivered() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let sid = "ses_file_oversize";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_big")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let over = crate::feishu::file::MAX_FILE_BYTES + 1;
        let transcript = SessionTranscript::new(vec![message(
            "a1",
            1_000,
            vec![read_file_part_sized(
                "call_read",
                "huge.bin",
                "application/octet-stream",
                b"%PDF",
                over,
            )],
        )]);
        render(&app, &cards, sid, &transcript).await;

        assert!(
            uploaded_files(&platform).await.is_empty(),
            "an over-cap file is never uploaded"
        );
        assert!(
            sent_messages(&platform).await.is_empty(),
            "an over-cap file is never sent"
        );
        let body = final_card(&platform).await["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(body.contains("· 未发送"), "{body}");
    }

    /// The ladder's first rung (#649): an embed upload that fails degrades to a
    /// File Message, so the image is still delivered — as a file — and the Turn
    /// is never failed.
    #[tokio::test]
    async fn an_embed_upload_failure_degrades_to_a_file_message() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        platform.given_file_key("file_v2_degraded");
        platform
            .fail_upload_image_count
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let sid = "ses_embed_degrade";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_degrade")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;
        let transcript = SessionTranscript::new(vec![message(
            "a1",
            1_000,
            vec![read_file_part("call_read", "shot.png", "image/png", b"ABC")],
        )]);

        let pass = render(&app, &cards, sid, &transcript).await;
        assert!(pass.is_some(), "a failed embed upload never fails the Turn");
        assert!(
            uploaded_images(&platform).await.is_empty(),
            "the failed embed upload records nothing"
        );
        assert_eq!(
            uploaded_files(&platform).await,
            vec![("shot.png".to_string(), 3)],
            "the image's bytes are uploaded as a file instead"
        );
        assert_eq!(
            sent_messages(&platform).await.len(),
            1,
            "exactly one File Message"
        );
        let card = final_card(&platform).await;
        assert!(
            card["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["tag"] != "img"),
            "no embed without a key: {card}"
        );
        let body = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(
            body.contains("📎 shot.png · image/png · 3 B · 已发送为文件消息"),
            "{body}"
        );
    }

    /// The ladder's last rung (#649): a File Message whose send fails degrades
    /// to `未发送`, best-effort — the card is built intact and the Turn is not
    /// failed.
    #[tokio::test]
    async fn a_file_message_send_failure_degrades_to_undelivered() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        platform
            .fail_send_message_count
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let sid = "ses_file_send_fail";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_send_fail")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;
        let transcript = SessionTranscript::new(vec![message(
            "a1",
            1_000,
            vec![read_file_part("call_read", "doc.pdf", "application/pdf", b"%PDF")],
        )]);

        let pass = render(&app, &cards, sid, &transcript).await;
        assert!(pass.is_some(), "a failed send never fails the Turn");
        assert_eq!(
            uploaded_files(&platform).await.len(),
            1,
            "the file still uploads before the send is attempted"
        );
        assert!(
            sent_messages(&platform).await.is_empty(),
            "the failed send records nothing"
        );
        let body = final_card(&platform).await["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(body.contains("· 未发送"), "{body}");
    }

    /// Identical bytes read by two calls upload once and post exactly ONE File
    /// Message (ADR-0076/#649): the content hash dedups both, so a turn reading
    /// the same file twice does not spam the thread.
    #[tokio::test]
    async fn identical_file_bytes_upload_and_send_once() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        platform.given_file_key("file_v2_same");
        let sid = "ses_file_dedup";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_file_dedup")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let transcript = SessionTranscript::new(vec![message(
            "a1",
            1_000,
            vec![
                read_file_part("call_read_1", "doc.pdf", "application/pdf", b"%PDF"),
                read_file_part("call_read_2", "doc.pdf", "application/pdf", b"%PDF"),
            ],
        )]);
        render(&app, &cards, sid, &transcript).await;

        assert_eq!(
            uploaded_files(&platform).await.len(),
            1,
            "identical bytes upload once"
        );
        assert_eq!(
            sent_messages(&platform).await.len(),
            1,
            "identical bytes post exactly one File Message"
        );
        let card = final_card(&platform).await;
        let lines = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "collapsible_panel")
            .filter(|e| {
                e["elements"][0]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("已发送为文件消息"))
            })
            .count();
        assert_eq!(lines, 2, "both panels show the sent line: {card}");
    }

    /// A user message's File Content is never delivered as a File Message
    /// (ADR-0076, #648): it is record-only, so the render path uploads and sends
    /// nothing for it.
    #[tokio::test]
    async fn a_user_file_part_is_never_sent_as_a_file_message() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let sid = "ses_user_file";
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_user_file")).await;
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;

        let content = crate::backend::FileContent::decode(
            "data:application/pdf;base64,JVBERi0=",
            Some("application/pdf"),
            Some("secret.pdf"),
        )
        .expect("an inline payload is a File Content");
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "u1",
                MessageRole::User,
                Some(0),
                vec![text_part("看看这个"), Part::File(content)],
            ),
            message("a1", 100, vec![text_part("看到了")]),
        ]);
        render(&app, &cards, sid, &transcript).await;

        assert!(
            uploaded_files(&platform).await.is_empty(),
            "a user message's file is never uploaded"
        );
        assert!(
            sent_messages(&platform).await.is_empty(),
            "a user message's file is never sent as a File Message"
        );
        let card = final_card(&platform).await.to_string();
        assert!(
            !card.contains("已发送为文件消息"),
            "the live card records no delivery for a user file: {card}"
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
        acc.set_reply_target(Some("msg_1".into()));
        acc.set_session("ses_test");
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
        let title = app
            .cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .title()
            .to_string();
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
        acc.set_reply_target(Some("msg_1".into()));
        acc.set_session("ses_test");
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
        acc.repoint_turn_anchor(&turn_anchor(turn_started));
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
        acc.set_card_state(CardState::Done);
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

    /// A text part with its server start time — the timeline key the cursor
    /// frontier records.
    fn text_at(text: &str, started_at: i64) -> Part {
        Part::Text(crate::backend::TextPart {
            text: text.to_string(),
            started_at: Some(started_at),
        })
    }

    /// Spec #561: building a card captures the Rendered Cursor frontier of the
    /// body-about-to-be-written — the newest text/reasoning part included,
    /// with its message identity, part position, kind, start time and
    /// delivered character extent. Staging it (and persisting on a confirmed
    /// write) is the flush's; this seam pins the capture.
    #[test]
    fn the_built_card_carries_the_delivered_frontier() {
        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));
        let text = "第一段回答。";
        let transcript = SessionTranscript::new(vec![message(
            "msg_a_1",
            100,
            vec![reasoning_at("先想一下", 90), text_at(text, 100)],
        )]);
        assert!(render_new_turn_parts(&mut acc, &transcript));

        let built = acc.build_card_with_info();
        assert_eq!(
            built.cursor.frontier,
            Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 1,
                kind: CursorPartKind::Text,
                started_at: Some(100),
                delivered_chars: text.chars().count(),
                prefix_digest: Some(cursor_prefix_digest(text)),
            }),
            "the frontier names the newest delivered text part and its extent"
        );
        assert!(built.cursor.live_calls.is_empty());
    }

    /// Spec #561, review #569: a settled tool delivered after the newest text
    /// part becomes the frontier itself — by position, with the text part's
    /// delivered extent carried along — so a restart never re-renders the
    /// panel. A still-running tool stays in the live set and never becomes the
    /// frontier.
    #[test]
    fn a_settled_tool_after_text_becomes_the_frontier() {
        let text = "回答";
        let running = SessionTranscript::new(vec![message(
            "msg_a_1",
            100,
            vec![
                text_at(text, 100),
                tool(
                    "bash",
                    "call_1",
                    ToolStatus::Running,
                    Some(150),
                    Some(serde_json::json!({ "command": "sleep 30" })),
                    None,
                ),
            ],
        )]);
        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));
        assert!(render_new_turn_parts(&mut acc, &running));

        // The running call stays display-only: the frontier is the text, the
        // call is in the live set.
        let built = acc.build_card_with_info();
        assert_eq!(
            built.cursor.frontier,
            Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: Some(100),
                delivered_chars: text.chars().count(),
                prefix_digest: Some(cursor_prefix_digest(text)),
            }),
            "a running tool never becomes the frontier"
        );
        assert_eq!(
            built.cursor.live_calls,
            ["call_1".to_string()].into_iter().collect()
        );

        // The call settles: the delivered panel is the newest item, so the
        // frontier names it — and still carries the text's delivered extent.
        let settled = SessionTranscript::new(vec![message(
            "msg_a_1",
            100,
            vec![
                text_at(text, 100),
                tool(
                    "bash",
                    "call_1",
                    ToolStatus::Completed,
                    Some(150),
                    Some(serde_json::json!({ "command": "sleep 30" })),
                    Some("done"),
                ),
            ],
        )]);
        assert!(render_new_turn_parts(&mut acc, &settled));
        let built = acc.build_card_with_info();
        assert_eq!(
            built.cursor.frontier,
            Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 1,
                kind: CursorPartKind::Tool,
                started_at: Some(150),
                delivered_chars: text.chars().count(),
                prefix_digest: Some(cursor_prefix_digest(text)),
            }),
            "the settled panel is the frontier, with the text extent carried"
        );
        assert!(built.cursor.live_calls.is_empty());
    }

    /// Spec #561: a part that spans more than one card records the delivered
    /// prefix on the finalized body and the cumulative extent on the
    /// continuation — the character position a later recovery resumes from.
    #[test]
    fn a_split_part_records_its_delivered_prefix_then_the_full_extent() {
        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));
        let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
        let text: String = "长".repeat(max + 500);
        let transcript = SessionTranscript::new(vec![message("msg_a_1", 100, vec![text_at(&text, 100)])]);
        assert!(render_new_turn_parts(&mut acc, &transcript));

        let first = acc.build_card_with_info();
        assert!(first.full, "the part overflows one card");
        assert_eq!(
            first.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(max),
            "the finalized body delivered only the first chunk"
        );
        let second = acc.build_card_with_info();
        assert!(!second.full);
        assert_eq!(
            second.cursor.frontier.as_ref().map(|f| f.delivered_chars),
            Some(text.chars().count()),
            "the continuation delivers the rest, cumulatively"
        );
        assert_eq!(
            second.cursor.frontier.as_ref().map(|f| f.part_index),
            Some(0),
            "both bodies name the same part"
        );
    }

    /// Spec #561: the live set follows the delivered body — a running call is
    /// in it, and a call settled into the timeline leaves it on the next
    /// delivered body.
    #[test]
    fn the_built_card_carries_running_tool_ids_and_drops_settled_ones() {
        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));
        let running = SessionTranscript::new(vec![message(
            "msg_a_1",
            100,
            vec![
                text_at("回答", 100),
                tool(
                    "bash",
                    "call_1",
                    ToolStatus::Running,
                    Some(150),
                    Some(serde_json::json!({ "command": "sleep 30" })),
                    None,
                ),
            ],
        )]);
        assert!(render_new_turn_parts(&mut acc, &running));

        let built = acc.build_card_with_info();
        assert_eq!(
            built.cursor.live_calls,
            ["call_1".to_string()].into_iter().collect(),
            "a running panel is in the delivered live set"
        );
        assert_eq!(
            built.cursor.frontier.as_ref().map(|f| f.message_id.as_str()),
            Some("msg_a_1"),
            "the text frontier is captured alongside"
        );

        // The call settles: the next body renders it as a timeline panel, so
        // the delivered live set drops it.
        let settled = SessionTranscript::new(vec![message(
            "msg_a_1",
            100,
            vec![
                text_at("回答", 100),
                tool(
                    "bash",
                    "call_1",
                    ToolStatus::Completed,
                    Some(150),
                    Some(serde_json::json!({ "command": "sleep 30" })),
                    Some("done"),
                ),
            ],
        )]);
        assert!(render_new_turn_parts(&mut acc, &settled));

        let built = acc.build_card_with_info();
        assert!(
            built.cursor.live_calls.is_empty(),
            "a settled call leaves the live set: {:?}",
            built.cursor.live_calls
        );
    }

    /// Spec #561: a body with no text/reasoning of its own keeps the chain's
    /// confirmed frontier — it must never regress to "nothing delivered".
    #[test]
    fn a_body_without_content_keeps_the_chains_frontier() {
        let mut acc = StreamAccumulator::new("test");
        acc.repoint_turn_anchor(&turn_anchor(0));
        let transcript =
            SessionTranscript::new(vec![message("msg_a_1", 100, vec![text_at("第一段。", 100)])]);
        assert!(render_new_turn_parts(&mut acc, &transcript));
        let built = acc.build_card_with_info();
        let frontier = built.cursor.frontier.expect("the text frontier");

        // The flush confirmed this frontier: mirror it into the base (a
        // confirmed write) and deliver a tool-only body afterwards.
        acc.set_cursor(RenderedCursor {
            frontier: Some(frontier.clone()),
            live_calls: Default::default(),
        });
        let tool_only = SessionTranscript::new(vec![message(
            "msg_a_2",
            200,
            vec![tool(
                "bash",
                "call_2",
                ToolStatus::Running,
                Some(250),
                Some(serde_json::json!({ "command": "sleep 30" })),
                None,
            )],
        )]);
        assert!(render_new_turn_parts(&mut acc, &tool_only));

        let built = acc.build_card_with_info();
        assert_eq!(
            built.cursor.frontier,
            Some(frontier),
            "the frontier does not regress on a text-free body"
        );
        assert_eq!(
            built.cursor.live_calls,
            ["call_2".to_string()].into_iter().collect()
        );
    }
    /// ADR-0071's frontier identity includes the part's server start time
    /// (spec #561, review #569): a replacement part in the same message slot
    /// with the same kind is a DIFFERENT part. The seed does not fail on it —
    /// the frontier is cut at 0, so the replacement renders in FULL while
    /// everything before it stays delivered — because dropping the seed would
    /// lose its tail entirely.
    #[test]
    fn a_replaced_part_in_the_frontier_slot_resolves_as_cut_zero() {
        let cursor = projection_cursor_at("msg_a_2000", 0, CursorPartKind::Text, Some(2_000), "答复", &[]);
        let replaced = SessionTranscript::new(vec![message(
            "msg_a_2000",
            2_000,
            // Same slot, same kind, longer than the delivered extent — only
            // the server start time differs.
            vec![text_at("另外的答复", 3_000)],
        )]);
        let seed = CursorSeed::resolve(&replaced, &cursor).expect("the slot resolves as cut zero");
        assert_eq!(
            seed.frontier.as_ref().map(|frontier| frontier.delivered_chars),
            Some(0),
            "a replaced part is cut at 0, never skipped"
        );
        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        acc.seed_projection(&cursor, seed);
        assert!(render_new_turn_parts(&mut acc, &replaced));
        let text = card_text(&acc.build_card_with_info().card);
        assert!(
            text.contains("另外的答复"),
            "the replacement renders in full: {text}"
        );

        let same = SessionTranscript::new(vec![message("msg_a_2000", 2_000, vec![text_at("答复", 2_000)])]);
        let seed = CursorSeed::resolve(&same, &cursor).expect("the recorded part still resolves");
        assert_eq!(
            seed.frontier.as_ref().map(|frontier| frontier.delivered_chars),
            Some(2),
            "the recorded part keeps its delivered extent"
        );
    }

    /// Only a live-set call the resolving read carries can be handed over
    /// (spec #561, review #569): a V2 read truncated at its page cap leaves a
    /// still-running call outside it, and the successor cannot render a panel
    /// it cannot see — the old card must keep that marker rather than strip
    /// it into nothing.
    #[test]
    fn a_live_set_call_outside_the_read_is_never_handed_over() {
        let transcript = SessionTranscript::new(vec![message(
            "msg_a_1",
            100,
            vec![
                text_at("回答", 100),
                tool(
                    "bash",
                    "call_seen",
                    ToolStatus::Running,
                    Some(150),
                    Some(serde_json::json!({ "command": "sleep 30" })),
                    None,
                ),
            ],
        )]);
        let cursor = RenderedCursor {
            frontier: None,
            live_calls: ["call_seen".to_string(), "call_gone".to_string()]
                .into_iter()
                .collect(),
        };
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("the cursor resolves");
        assert_eq!(
            seed.resolved_calls(),
            vec!["call_seen".to_string()],
            "a named call outside the read is never handed over"
        );

        // Every named call present: the successor renders them, so the old
        // card's running markers may go.
        let all_present = RenderedCursor {
            live_calls: ["call_seen".to_string()].into_iter().collect(),
            ..cursor.clone()
        };
        let seed = CursorSeed::resolve(&transcript, &all_present).expect("the cursor resolves");
        assert_eq!(
            seed.resolved_calls(),
            vec!["call_seen".to_string()],
            "every named call present resolves"
        );
    }

    /// Spec #561, review #569: V2 decodes text parts without `time.start`, so
    /// two parts in the same slot are indistinguishable by identity alone. The
    /// frontier's prefix digest tells growth from replacement: a part grown in
    /// place keeps its delivered prefix, so the seed resolves and renders only
    /// the tail.
    #[test]
    fn a_timestampless_part_grown_in_place_still_resolves() {
        let prefix = "第一段回答。";
        let full = format!("{prefix}第二段回答。");
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_anchor",
                MessageRole::User,
                Some(1_000),
                vec![text_part("问题")],
            ),
            message("msg_a_2000", 2_000, vec![text_part(&full)]),
        ]);
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, prefix, &[]);
        let seed = CursorSeed::resolve(&transcript, &cursor).expect("growth keeps the prefix");

        let mut acc = StreamAccumulator::new("proj");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        acc.seed_projection(&cursor, seed);
        assert!(render_new_turn_parts(&mut acc, &transcript));
        let text = card_text(&acc.build_card_with_info().card);
        assert!(
            text.contains("第二段回答。"),
            "the undelivered tail renders: {text}"
        );
        assert!(
            !text.contains(prefix),
            "the delivered prefix is not repeated: {text}"
        );
    }

    /// A timestampless REPLACEMENT with different content resolves as CUT 0
    /// (spec #561, review #569): the digest mismatches, so the slot is not the
    /// recorded part — but failing the seed would lose its tail, so the
    /// frontier renders it in full instead. The same content in the same slot
    /// is growth, not a replacement, and keeps its delivered extent.
    #[test]
    fn a_timestampless_replacement_resolves_as_cut_zero() {
        let cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, "答复", &[]);
        let replaced =
            SessionTranscript::new(vec![message("msg_a_2000", 2_000, vec![text_part("另外的答复")])]);
        let seed = CursorSeed::resolve(&replaced, &cursor).expect("the slot resolves as cut zero");
        assert_eq!(
            seed.frontier.as_ref().map(|frontier| frontier.delivered_chars),
            Some(0),
            "a digest mismatch cuts the part at 0 rather than skipping it"
        );
        let same = SessionTranscript::new(vec![message("msg_a_2000", 2_000, vec![text_part("答复")])]);
        let seed = CursorSeed::resolve(&same, &cursor).expect("the same content resolves");
        assert_eq!(
            seed.frontier.as_ref().map(|frontier| frontier.delivered_chars),
            Some(2),
            "the recorded content keeps its delivered extent"
        );
    }

    /// A cursor written by an older release carries no prefix digest: the seed
    /// falls back (no projection) rather than guess — the one-release
    /// migration seam (spec #561, review #569).
    #[test]
    fn a_cursor_without_a_prefix_digest_falls_back() {
        let mut cursor = projection_cursor("msg_a_2000", 0, CursorPartKind::Text, "答复", &[]);
        cursor.frontier.as_mut().expect("a frontier").prefix_digest = None;
        let transcript =
            SessionTranscript::new(vec![message("msg_a_2000", 2_000, vec![text_part("答复。")])]);
        assert!(
            CursorSeed::resolve(&transcript, &cursor).is_none(),
            "no digest, no projection"
        );
    }

    /// The message-first race's seed (spec #561, ticket #565, review #569): a
    /// frontier whose recorded prefix digest no longer matches the read — the
    /// orphan's part was rewritten in place — still delivers the rewritten
    /// part IN FULL before the new Turn's content, instead of falling back to
    /// the live set alone and losing the tail.
    #[test]
    fn a_message_first_seed_renders_a_rewritten_orphan_part_in_full() {
        let new_anchor = 2_000_000;
        let orphan = TurnAnchor {
            message_id: MessageId::new("msg_cola_old"),
            created_ms: new_anchor - 60_000,
        };
        let rewritten = "改写后的完整回答。";
        let transcript = SessionTranscript::new(vec![
            typed_message(
                "msg_cola_new",
                MessageRole::User,
                Some(new_anchor),
                vec![text_part("新的问题")],
            ),
            message_in_flight("a_orphan", new_anchor - 50_000, None, vec![text_part(rewritten)]),
        ]);
        // The cursor recorded a prefix that no longer exists in the read.
        let cursor = projection_cursor_at("a_orphan", 0, CursorPartKind::Text, None, "改写前的前缀", &[]);
        let seed =
            CursorSeed::for_orphan_resolving(&transcript, &cursor, &orphan).expect("the cursor resolves");
        assert!(
            seed.frontier.is_some(),
            "a digest mismatch resolves as cut zero, not a fallback"
        );

        let mut acc = StreamAccumulator::new("proj");
        acc.set_cola_message_id("msg_cola_new");
        acc.seed_projection(&cursor, seed);
        assert!(render_new_turn_parts(&mut acc, &transcript));
        let text = card_text(&acc.build_card_with_split().0);
        assert!(
            text.contains(rewritten),
            "the rewritten orphan answer renders in full: {text}"
        );
    }

    /// Spec #561, review #569: a part the server REWRITES (a new snapshot that
    /// does not extend the tracked content) replaces its timeline entry run
    /// instead of accumulating the old one — the card shows the replacement
    /// exactly once and the cursor records its digest at the new full length,
    /// so a restart resolves it instead of falling back as the legacy case.
    #[test]
    fn a_rewritten_part_replaces_its_entry_run_and_stamps_the_cursor() {
        let transcript = |text: &str| {
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_anchor",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("问题")],
                ),
                message("msg_a_2000", 2_000, vec![text_at(text, 2_000)]),
            ])
        };
        let mut acc = StreamAccumulator::new("t");
        acc.repoint_turn_anchor(&turn_anchor(1_000));
        assert!(render_new_turn_parts(&mut acc, &transcript("ABC")));
        // The server rewrote the part in place.
        assert!(render_new_turn_parts(&mut acc, &transcript("XYZ")));

        let built = acc.build_card_with_info();
        let text = card_text(&built.card);
        assert_eq!(
            text.matches("XYZ").count(),
            1,
            "the replacement renders once: {text}"
        );
        assert!(
            !text.contains("ABC"),
            "the rewritten-away content is not accumulated: {text}"
        );
        assert_eq!(
            built.cursor.frontier,
            Some(CursorFrontier {
                message_id: MessageId::new("msg_a_2000"),
                part_index: 0,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: 3,
                prefix_digest: Some(cursor_prefix_digest("XYZ")),
            }),
            "the cursor records the replacement's full extent and digest"
        );

        // The persisted cursor resolves against the rewritten read (nothing
        // undelivered) and against a grown one (only the tail).
        let seed = CursorSeed::resolve(&transcript("XYZ"), &built.cursor).expect("the cursor resolves");
        assert_eq!(
            seed.frontier.as_ref().map(|frontier| frontier.delivered_chars),
            Some(3)
        );
        let grown = CursorSeed::resolve(&transcript("XYZ更多"), &built.cursor).expect("the cursor resolves");
        assert_eq!(
            grown.frontier.as_ref().map(|frontier| frontier.delivered_chars),
            Some(3),
            "growth still renders only the tail"
        );
    }
}
