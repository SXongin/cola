use std::sync::Arc;

use crate::bridge::card_handles::RenderedBlock;
use crate::bridge::core::{SESSION_INFO_TIMEOUT, SharedCore};
use crate::bridge::streaming::StreamAccumulator;

/// The session/thread name shown as the card subtitle, formatted as
/// `<title> · <id-tail>` (e.g. "你好 · 01ba0ed"). The OpenCode server's OWN
/// session title (what OpenChamber shows) is the single source of truth
/// (ADR-0007) — fetched on demand, never cached locally. While the server
/// still has the default `New session - ...` title (or the title is empty),
/// the id-tail alone identifies the session; the current prompt is never
/// echoed (the reply context already shows it).
pub(crate) async fn session_subtitle(
    core: &SharedCore,
    thread_key: &crate::config::ThreadKey,
    text: &str,
) -> String {
    let prompt_preview: String = text.chars().take(50).collect();
    let entry = {
        let store = core.sessions.lock().await;
        store.get_active(thread_key).cloned()
    };
    let Some(entry) = entry else {
        return String::new();
    };
    let session_id = entry.session_id.clone();
    let mut name = String::new();
    if let Ok(Ok(info)) = tokio::time::timeout(
        SESSION_INFO_TIMEOUT,
        core.opencode
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
pub(crate) async fn refresh_session_title(core: &Arc<SharedCore>, session_id: &str) -> bool {
    // Only meaningful while a turn is actively streaming on a live card.
    let acc_present = core.cards.lock().await.contains_key(session_id);
    if !acc_present {
        return false;
    }
    let thread_key = core.sessions.lock().await.thread_for_session(session_id);
    let Some(thread_key) = thread_key else {
        return false;
    };
    // The subtitle is formatted with the session id-tail; recompute it now
    // and keep whatever the server currently reports.
    let fresh = session_subtitle(core, &thread_key, "").await;
    if fresh.is_empty() {
        return false;
    }
    let mut cards = core.cards.lock().await;
    let Some(card) = cards.get_mut(session_id) else {
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
    drop(cards);
    flush_card(core, session_id).await;
    // The topic cover card is the chat-list topic entry — sync it the MOMENT
    // the server title changes mid-turn (not only at turn end), so the list
    // entry updates as early as the title agent finishes (ADR-0023). Only
    // fires on this change tick; the per-tick cost is unchanged.
    crate::bridge::topic::sync_topic_cover_title(core, session_id).await;
    true
}

/// Extract the user-visible output of a tool part. Tries the historical
/// `state.output` / `state.metadata.output`, then the current schema:
/// `state.content[*].text` joined, `state.result` (string), and for an "error"
/// status the `state.error` — which may be a string (`"Could not find ..."`) or
/// an object (`{"message": "..."}`).
fn extract_tool_output(part: &serde_json::Value, status: &str) -> Option<String> {
    if let Some(o) = part
        .get("state")
        .and_then(|s| s.get("output"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    {
        return Some(o);
    }
    let state = part.get("state")?;
    let mut out = String::new();
    if let Some(arr) = state.get("content").and_then(|c| c.as_array()) {
        for item in arr {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                out.push_str(t);
            }
        }
    }
    if let Some(r) = state.get("result").and_then(|v| v.as_str()) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(r);
    }
    if status == "error"
        && let Some(e) = state.get("error").and_then(|e| match e {
            // Object form: `{"message": "..."}`.
            serde_json::Value::Object(m) => m.get("message").and_then(|v| v.as_str()),
            // Plain string form: `"Could not find oldString..."`.
            serde_json::Value::String(s) => Some(s.as_str()),
            _ => None,
        })
    {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("❌ {}", e));
    }
    if out.is_empty() {
        // Historical fallback: `state.metadata.output`.
        state
            .get("metadata")
            .and_then(|m| m.get("output"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        Some(out)
    }
}

/// For an `edit` tool, prefer the REAL diff recorded in `state.metadata.diff`
/// (OpenCode computes it with `createTwoFilesPatch`) over the tool's plain
/// text output ("Edit applied successfully."), which tells the reader nothing
/// about what changed. `extract_tool_output` only reads
/// `state.output/content/result`, so without this the diff — the actual
/// interesting content of every file edit — was silently dropped and the card
/// just repeated the file name. Failures keep their extracted error text.
fn edit_tool_output(part: &serde_json::Value, status: &str) -> Option<String> {
    let orig = extract_tool_output(part, status);
    if status == "error" {
        return orig;
    }
    let diff = part
        .pointer("/state/metadata/diff")
        .and_then(|v| v.as_str())
        .filter(|d| !d.is_empty())?;
    // The success sentence is noise once the diff is shown; anything beyond it
    // (e.g. an "LSP errors detected" note) is kept as a tail after the diff.
    let tail = orig
        .as_deref()
        .and_then(|o| o.strip_prefix("Edit applied successfully."))
        .map(|s| s.trim_start_matches('\n'))
        .filter(|s| !s.is_empty());
    Some(match tail {
        Some(t) => format!("{diff}\n\n{t}"),
        None => diff.to_string(),
    })
}

/// Render canonical message parts (from `POST /session/{id}/message` response)
/// into the accumulator so the card shows the assistant's final result.
/// The server-side start time (epoch ms) of the part — its timeline key, and
/// the only clock the card may show. Text/reasoning carry it at
/// `/time/start`; a tool's state carries it at `/state/time/start`.
/// Step/patch parts have none (they render nothing), and older payloads / test
/// fixtures may omit it: the part is then keyed by a monotonic fallback
/// (call order) and shows no clock.
fn part_time(part: &serde_json::Value) -> Option<i64> {
    part.pointer("/time/start")
        .or_else(|| part.pointer("/state/time/start"))
        .and_then(|v| v.as_i64())
}

fn render_part(acc: &mut StreamAccumulator, part: &serde_json::Value) {
    // The part's server start time, if the payload carries one: it both places
    // the item on the timeline and stamps a panel header.
    let at = part_time(part);
    match part.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.push_text_at(at, t);
            }
            acc.card_state = crate::feishu::card::CardState::Streaming;
        }
        Some("reasoning") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.push_reasoning_at(at, t);
            }
            acc.card_state = crate::feishu::card::CardState::Reasoning;
        }
        Some("tool") => {
            let name = part.get("tool").and_then(|v| v.as_str()).unwrap_or("tool");
            let call_id = part
                .get("callID")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string();
            let status = part
                .get("state")
                .and_then(|s| s.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let input = part.get("state").and_then(|s| s.get("input")).cloned();
            // OpenCode stores tool output as `state.content` (array of
            // {type:"text",text}) plus an optional `result`, and failures put
            // the reason in `state.error.message`. There is NO `state.output`
            // field on tool parts — reading it silently lost every result. An
            // `edit` call additionally records its unified diff in
            // `state.metadata.diff`, which is what the panel should show.
            let output = if name == "edit" {
                edit_tool_output(part, status)
            } else {
                extract_tool_output(part, status)
            };
            let panel = crate::feishu::card::tool_render::ToolPanel {
                name: name.to_string(),
                status: status.to_string(),
                input,
                output,
            };
            if name == "todowrite" {
                // A live status section, not a transcript row: the latest call
                // replaces the panel the card tail renders (on the live card,
                // so a split can't strand an outdated list). The clock is
                // always reassigned — a payload without a server time shows no
                // clock rather than the previous call's, which would read as a
                // write time this list never had.
                acc.todo_panel = Some(panel);
                acc.todo_shown_at = at;
            } else {
                acc.push_tool_at(at, &call_id, panel);
            }
            if status == "running" {
                acc.card_state = crate::feishu::card::CardState::Streaming;
            }
        }
        Some("step-start") | Some("step-finish") | Some("patch") => {
            // No visible content for these
        }
        _ => {}
    }
    // card_state / running-tool changes reset the header phase timer.
    acc.refresh_phase();
}

/// Render a batch of parts into the accumulator, skipping anything already
/// rendered (same dedup as the poll loop). Returns true if anything new was
/// rendered. Used as the final fallback when the incremental poll missed parts.
pub(crate) fn render_parts(acc: &mut StreamAccumulator, parts: &serde_json::Value) -> bool {
    let Some(arr) = parts.as_array() else { return false };
    let mut rendered_any = false;
    for part in arr {
        if render_part_once(acc, part) {
            rendered_any = true;
        }
    }
    rendered_any
}

/// Render a single part into the accumulator, applying the same dedup rules as
/// the poll loop: reasoning/text are tracked by `{type}:{content}` (OpenCode
/// part payloads carry NO `id`), tool parts by a state signature so they can
/// re-render on running → completed, everything else by part `id`. Returns true
/// if the part was rendered (not skipped as duplicate/empty).
fn render_part_once(acc: &mut StreamAccumulator, part: &serde_json::Value) -> bool {
    let ptype = part
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    // Reasoning/text parts are written with empty text first, then updated with
    // the full content. Only render once they have content, otherwise we'd
    // freeze the placeholder version.
    if ptype == "reasoning" || ptype == "text" {
        let Some(t) = part.get("text").and_then(|v| v.as_str()) else {
            return false;
        };
        if t.is_empty() {
            return false;
        }
        let dedup_key = format!("{}:{}", ptype, t);
        if acc.rendered_parts.contains(&dedup_key) {
            return false;
        }
        acc.rendered_parts.insert(dedup_key);
        render_part(acc, part);
        return true;
    }
    // Tool parts get updated in place (running → completed); re-render whenever
    // the state signature changes so panels don't stay stuck on "running".
    if ptype == "tool" {
        let call_id = part.get("callID").and_then(|v| v.as_str()).unwrap_or_default();
        let status = part
            .pointer("/state/status")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let output_len = part
            .pointer("/state/output")
            .and_then(|v| v.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        // An edit's meaningful content is its unified diff (`metadata.diff`),
        // not `state.output` ("Edit applied successfully.") — fold its length
        // into the signature so a diff appearing under a stable status still
        // triggers a re-render.
        let diff_len = part
            .pointer("/state/metadata/diff")
            .and_then(|v| v.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        // A todowrite's panel is replaced on every call, so its signature must
        // fold in the state CONTENT: a list whose items changed without
        // changing any length (任务 A → 任务 B) must still refresh the panel.
        let sig = if part.get("tool").and_then(|v| v.as_str()) == Some("todowrite") {
            part.get("state").map(|s| s.to_string()).unwrap_or_default()
        } else {
            format!("{status}|{output_len}|{diff_len}")
        };
        if acc.rendered_tool_states.get(call_id) == Some(&sig) {
            return false;
        }
        acc.rendered_tool_states.insert(call_id.to_string(), sig);
        render_part(acc, part);
        return true;
    }
    // Everything else (step-start/step-finish/patch): render once.
    let part_id = part.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
    if let Some(id) = &part_id {
        if acc.rendered_parts.contains(id) {
            return false;
        }
        acc.rendered_parts.insert(id.clone());
    }
    render_part(acc, part);
    true
}

/// Capture the turn's SERVER-clock anchor (`turn_started_ms`) from the user
/// message the server stored, matched by the `msg_cola_` id cola chose
/// (ADR-0026). External renders arm with the anchor directly; this fills it in
/// for cola's own turns on the first poll that sees the message. It must run
/// before any filtering: the anchor alone decides which messages are this
/// turn's, and cola's clock cannot.
fn capture_turn_anchor(acc: &mut StreamAccumulator, msgs: &[crate::opencode::types::SessionMessage]) {
    if acc.turn_started_ms.is_some() {
        return;
    }
    let Some(cola_message_id) = acc.cola_message_id.as_deref() else {
        return;
    };
    for m in msgs {
        if m.info.role.as_deref() == Some("user")
            && m.info.id == cola_message_id
            && let Some(t) = m.info.time.as_ref()
        {
            acc.turn_started_ms = Some(t.created);
            return;
        }
    }
}

/// Render the parts of this turn's assistant messages that haven't been
/// rendered yet. Returns true if anything new was rendered.
///
/// The turn filter is anchored on `acc.turn_started_ms`, the SERVER's own time
/// for this turn's user message (#190). Filtering against cola's submit clock
/// instead dropped the new turn's parts when the server ran behind cola, and
/// bled the previous turn's parts into the new card when it ran ahead. Until
/// the anchor is observed nothing renders: with two skewed clocks there is no
/// threshold that tells the two turns apart.
pub(crate) fn render_new_turn_parts(
    acc: &mut StreamAccumulator,
    msgs: &[crate::opencode::types::SessionMessage],
) -> bool {
    capture_turn_anchor(acc, msgs);
    let Some(anchor_ms) = acc.turn_started_ms else {
        return false;
    };
    let mut rendered_any = false;
    for m in msgs {
        let is_assistant = m.info.role.as_deref() == Some("assistant");
        let in_turn = m
            .info
            .time
            .as_ref()
            .map(|t| t.created >= anchor_ms)
            .unwrap_or(false);
        if !is_assistant || !in_turn {
            continue;
        }
        // Capture the answering model + token usage for the card footer.
        if let Some(model_id) = &m.info.model_id {
            acc.model_id = Some(model_id.clone());
        }
        if let Some(provider_id) = &m.info.provider_id {
            acc.provider_id = Some(provider_id.clone());
        }
        if let Some(tokens) = &m.info.tokens {
            acc.context_tokens = tokens.context_used();
        }
        let Some(parts) = m.parts.as_array() else { continue };
        for part in parts {
            if render_part_once(acc, part) {
                rendered_any = true;
            }
        }
    }
    rendered_any
}

/// Push the accumulator's current card to Feishu as an update of the loading
/// card, so the user sees reasoning/tool/text appear incrementally.
/// Upper bound on continuation cards sent for one flush (each is a new message).
const MAX_CARD_CHAIN: usize = 8;

pub(crate) async fn flush_card(core: &Arc<SharedCore>, session_id: &str) {
    // One card writer per session at a time. A flush is a read-send-record
    // sequence, and callers are concurrent (the render poll, the request
    // poller surfacing a block, a click's ack fallback); interleaved, the
    // second writer still names the card the first just finalized and PATCHes
    // its continuation slice onto it — two identical messages, only one
    // tracked and repaintable. The resolution paths take the same lock
    // (`resolve_blocks`), so a click cannot be overwritten by a stale flush.
    let write_lock = core.card_write_lock(session_id).await;
    let _guard = write_lock.lock().await;
    flush_card_locked(core, session_id).await;
}

async fn flush_card_locked(core: &Arc<SharedCore>, session_id: &str) {
    // Is the accumulator's current card still the live (growing) card? It is
    // until the first split finalizes it. Every card after that was sent as a
    // FINALIZED slice (its build was already over the budget) and must never be
    // overwritten: an update with the next slice would drop the content — and
    // the Interaction Receipts — that card carries.
    let mut card_is_live = true;
    for _ in 0..MAX_CARD_CHAIN {
        let (built, rendered) = {
            let mut cards = core.cards.lock().await;
            let Some(card) = cards.get_mut(session_id) else {
                return;
            };
            let built = card.acc.build_card_with_info();
            let rendered = built
                .spans
                .iter()
                .filter_map(|span| {
                    card.acc
                        .interaction(&span.request_id)
                        .and_then(|block| RenderedBlock::of(span, block))
                })
                .collect();
            (built, rendered)
        };
        let card_id = {
            let cards = core.cards.lock().await;
            cards.get(session_id).and_then(|c| c.card_message_id.clone())
        };
        let Some(card_id) = card_id else { return };

        if card_is_live {
            // The live card takes the slice in place: either it still fits
            // (plain update) or this flush finalizes it with the "to be
            // continued" marker.
            if let Err(e) = core.feishu.update_message(&card_id, &built.card).await {
                tracing::warn!("Card update failed: {}", e);
            }
            // Record what this card now renders: the live blocks (or none, when
            // the update finalized the card without its tail — the continuation
            // below then takes them over).
            core.card_handles
                .lock()
                .await
                .record(&card_id, &built.card, rendered);
            if !built.full {
                return;
            }
            card_is_live = false;
            continue;
        }

        // A finalized slice needs its own card; `build_card_with_info` already
        // advanced `render_from` past the split point, so each continuation
        // holds exactly the slice it was built from — nothing is lost.
        let reply_to = {
            let cards = core.cards.lock().await;
            cards
                .get(session_id)
                .and_then(|c| c.acc.reply_to_message_id.clone())
        };
        let Some(reply_to) = reply_to else { return };
        match core.feishu.reply_card(&reply_to, &built.card).await {
            Ok(new_id) => {
                {
                    let mut cards = core.cards.lock().await;
                    if let Some(card) = cards.get_mut(session_id) {
                        card.card_message_id = Some(new_id.clone());
                    }
                }
                // The continuation takes the blocks over from the finalized
                // slice it follows (its spans are the tail this card renders).
                core.card_handles
                    .lock()
                    .await
                    .record(&new_id, &built.card, rendered);
                if !built.full {
                    return;
                }
                // The continuation is itself over the limit → loop for the
                // next slice's card.
            }
            Err(e) => {
                tracing::warn!("Card continuation send failed: {}", e);
                return;
            }
        }
    }
}

/// Poll the session's messages and render any new parts into the streaming
/// card, flushing when something changed. The shared heart of both render
/// loops — `render_poll_loop` (cola's own prompts) and the external-message
/// renderer (`bridge::external`) — so the two never drift apart.
///
/// Returns `Some((new_parts, text_len, reasoning_len))` when the accumulator is
/// still present (the statistics are for logging); `None` when it vanished (the
/// caller should stop).
pub(crate) async fn render_and_flush(
    core: &Arc<SharedCore>,
    session_id: &str,
    msgs: &[crate::opencode::types::SessionMessage],
) -> Option<(usize, usize, usize)> {
    // OpenCode auto-renames sessions after a turn; follow the server's live
    // title so the card subtitle doesn't stay on the "new session" default.
    refresh_session_title(core, session_id).await;
    let (changed, header_changed, new_parts, text_len, reasoning_len) = {
        let mut cards = core.cards.lock().await;
        let card = cards.get_mut(session_id)?;
        let before = card.acc.rendered_parts.len();
        let changed = render_new_turn_parts(&mut card.acc, msgs);
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
        )
    };
    if changed || header_changed {
        flush_card(core, session_id).await;
    }
    Some((new_parts, text_len, reasoning_len))
}

/// Incremental renderer: while the synchronous prompt is in flight, poll the
/// session's messages and flush the card as parts complete (reasoning, tools,
/// text). `done` stops the loop once the prompt returns.
pub(crate) async fn render_poll_loop(
    core: &Arc<SharedCore>,
    session_id: String,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
        if done.load(Ordering::SeqCst) {
            return;
        }
        let msgs = match core.opencode.messages(&session_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("render poll messages: {}", e);
                continue;
            }
        };
        match render_and_flush(core, &session_id, &msgs).await {
            // Accumulator gone (turn completed and was cleaned up); keep polling
            // until the prompt returns so late parts are still caught.
            None => continue,
            Some((new_parts, text_len, reasoning_len)) => {
                if new_parts > 0 {
                    tracing::info!(
                        "render poll: {} new parts, text={} reasoning={}",
                        new_parts,
                        text_len,
                        reasoning_len
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::streaming::StreamAccumulator;
    use crate::feishu::card::CardState;

    #[test]
    fn render_part_marks_content_and_tracks_header_phase() {
        use crate::bridge::streaming::HeaderPhase;
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);
        assert_eq!(acc.current_phase, Some(HeaderPhase::Loading));

        let msgs = vec![SessionMessage {
            info: MessageInfo {
                id: "a1".into(),
                role: Some("assistant".into()),
                parent_id: None,
                time: Some(MessageTime { created: 100 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([
                { "type": "reasoning", "text": "Let me think" },
                { "type": "text", "text": "Answer" },
            ]),
        }];

        assert!(render_new_turn_parts(&mut acc, &msgs));
        assert_eq!(acc.current_phase, Some(HeaderPhase::Streaming));
    }

    #[test]
    fn render_parts_shows_reasoning_and_tool_output() {
        // Shapes copied from a real turn in the message store: reasoning parts
        // carry "text", tool parts carry "state.output" (NOT state.metadata.output).
        let parts = serde_json::json!([
            {"type": "step-start", "snapshot": "abc"},
            {"type": "reasoning", "text": "The user is asking in Chinese."},
            {"type": "tool", "tool": "bash", "callID": "call_1",
             "state": {"status": "completed", "input": {"command": "pwd && ls -la"},
                       "output": "/root/workspace/dev/cola\n..."}},
            {"type": "step-finish", "reason": "tool-calls"},
            {"type": "step-start", "snapshot": "abc"},
            {"type": "text", "text": "我是 opencode。"},
            {"type": "step-finish", "reason": "stop"},
        ]);

        let mut acc = StreamAccumulator::new("test");
        render_parts(&mut acc, &parts);
        acc.card_state = CardState::Done;

        assert!(acc.reasoning.contains("The user is asking in Chinese."));
        assert_eq!(acc.tools.len(), 1);
        let tool = &acc.tools["call_1"];
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.status, "completed");
        assert!(
            tool.output
                .as_deref()
                .unwrap()
                .contains("/root/workspace/dev/cola")
        );
        assert!(
            tool.input
                .as_ref()
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
        let parts = serde_json::json!([
            {"type": "tool", "tool": "read", "callID": "call_2",
             "state": {"status": "completed", "input": {"path": "src/main.rs"},
                       "metadata": {"output": "fn main() {}"}}},
        ]);
        let mut acc = StreamAccumulator::new("test");
        render_parts(&mut acc, &parts);
        assert_eq!(acc.tools["call_2"].output.as_deref(), Some("fn main() {}"));
    }

    #[test]
    fn render_new_turn_parts_filters_turn_and_dedups() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        // The user message's server time is the turn anchor; the old assistant
        // (created 100) sits before it, the current one (3000) after.
        let anchor = 2000;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(anchor);

        let msgs = vec![
            // Old turn assistant message (before the anchor) — skipped.
            SessionMessage {
                info: MessageInfo {
                    id: "old".into(),
                    role: Some("assistant".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: 100 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "id": "prt_old", "type": "reasoning", "text": "old reasoning" }]),
            },
            // User message — skipped (not assistant).
            SessionMessage {
                info: MessageInfo {
                    id: "user".into(),
                    role: Some("user".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: 2000 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "id": "prt_user", "type": "text", "text": "question" }]),
            },
            // Current turn assistant message.
            SessionMessage {
                info: MessageInfo {
                    id: "a1".into(),
                    role: Some("assistant".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: 3000 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([
                    { "id": "prt_rsn", "type": "reasoning", "text": "Let me think" },
                    { "id": "prt_tool", "type": "tool", "tool": "bash", "callID": "call_1", "state": { "status": "completed", "input": { "command": "ls" }, "output": "src" } },
                ]),
            },
        ];

        assert!(render_new_turn_parts(&mut acc, &msgs));
        assert!(acc.reasoning.contains("Let me think"));
        assert_eq!(acc.tools.len(), 1);
        assert_eq!(acc.rendered_parts.len(), 1);
        assert_eq!(acc.rendered_tool_states.len(), 1);
        assert!(!acc.text.contains("question"));
        assert!(!acc.reasoning.contains("old reasoning"));

        assert!(!render_new_turn_parts(&mut acc, &msgs));
    }

    /// The header date reads the SERVER's time for the turn's user message
    /// (#183 follow-up): captured on the first poll that sees it, so cola's
    /// own clock never reaches the card. The same captured anchor is #190's
    /// turn filter, so this test also pins the capture source.
    #[test]
    fn turn_started_ms_captures_the_user_message_server_time() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let msgs = vec![SessionMessage {
            info: MessageInfo {
                id: "msg_cola_1".into(),
                role: Some("user".into()),
                parent_id: None,
                time: Some(MessageTime { created: 1234 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([{ "type": "text", "text": "你好" }]),
        }];

        assert!(!render_new_turn_parts(&mut acc, &msgs));
        assert_eq!(acc.turn_started_ms, Some(1234));
    }

    /// Until the server's own user message is observed there is no anchor, and
    /// nothing renders: cola's clock is no substitute — with skewed clocks any
    /// threshold it provides either drops the new turn or admits the old one.
    #[test]
    fn nothing_renders_before_the_server_anchor_is_observed() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let msgs = vec![SessionMessage {
            info: MessageInfo {
                id: "a1".into(),
                role: Some("assistant".into()),
                parent_id: Some("msg_cola_1".into()),
                time: Some(MessageTime { created: 100 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([{ "type": "text", "text": "回答" }]),
        }];

        assert!(!render_new_turn_parts(&mut acc, &msgs));
        assert_eq!(acc.turn_started_ms, None);
        assert!(acc.text.is_empty());
    }

    /// #190: a server clock BEHIND cola's must not drop the new turn — every
    /// part is "in the past" for cola, so the retired cola-clock filter
    /// skipped them and the card stayed empty. The filter anchors on the
    /// server's own user-message time instead.
    #[test]
    fn a_server_clock_behind_cola_does_not_drop_the_turns_parts() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let cola_now = chrono::Utc::now().timestamp_millis();
        let server_user = cola_now - 3_600_000; // the server is an hour behind
        let assistant = server_user + 250;

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let msgs = vec![
            SessionMessage {
                info: MessageInfo {
                    id: "msg_cola_1".into(),
                    role: Some("user".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: server_user }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "type": "text", "text": "你好" }]),
            },
            SessionMessage {
                info: MessageInfo {
                    id: "a1".into(),
                    role: Some("assistant".into()),
                    parent_id: Some("msg_cola_1".into()),
                    time: Some(MessageTime { created: assistant }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "type": "text", "text": "回答" }]),
            },
        ];
        assert!(
            assistant < cola_now,
            "fixture: the whole turn predates cola's clock"
        );

        assert!(render_new_turn_parts(&mut acc, &msgs));
        assert_eq!(acc.turn_started_ms, Some(server_user));
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
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let cola_now = chrono::Utc::now().timestamp_millis();
        let previous_assistant = cola_now + 30_000; // still future to cola
        let server_user = cola_now + 60_000; // this turn's user message
        let assistant = server_user + 250;

        let mut acc = StreamAccumulator::new("proj");
        acc.cola_message_id = Some("msg_cola_1".into());
        let msgs = vec![
            SessionMessage {
                info: MessageInfo {
                    id: "a_old".into(),
                    role: Some("assistant".into()),
                    parent_id: Some("msg_cola_old".into()),
                    time: Some(MessageTime {
                        created: previous_assistant,
                    }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "type": "text", "text": "旧回答" }]),
            },
            SessionMessage {
                info: MessageInfo {
                    id: "msg_cola_1".into(),
                    role: Some("user".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: server_user }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "type": "text", "text": "你好" }]),
            },
            SessionMessage {
                info: MessageInfo {
                    id: "a1".into(),
                    role: Some("assistant".into()),
                    parent_id: Some("msg_cola_1".into()),
                    time: Some(MessageTime { created: assistant }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "type": "text", "text": "新回答" }]),
            },
        ];
        assert!(
            previous_assistant > cola_now,
            "fixture: the previous turn is still ahead of cola's clock"
        );

        assert!(render_new_turn_parts(&mut acc, &msgs));
        assert_eq!(acc.turn_started_ms, Some(server_user));
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
            &serde_json::json!([
                { "type": "reasoning", "text": "thinking", "time": { "start": at } },
                { "type": "tool", "tool": "bash", "callID": "call_1",
                  "state": { "status": "pending" } },
            ]),
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
            &serde_json::json!([
                { "type": "tool", "tool": "bash", "callID": "call_1",
                  "state": { "status": "running", "input": { "command": "sleep 2" },
                             "time": { "start": at } } },
            ]),
        );
        let card = acc.build_card().to_string();
        assert!(card.contains("⏳ bash · 00:05"), "{card}");
    }

    #[test]
    fn tool_part_update_re_renders_panel() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);

        let msgs = |status: &str, output: &str| {
            vec![SessionMessage {
                info: MessageInfo {
                    id: "a1".into(),
                    role: Some("assistant".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: 100 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{
                    "id": "prt_tool",
                    "type": "tool",
                    "tool": "bash",
                    "callID": "call_1",
                    "state": { "status": status, "input": { "command": "ls" }, "output": output },
                }]),
            }]
        };

        // First render: tool running.
        assert!(render_new_turn_parts(&mut acc, &msgs("running", "")));
        assert_eq!(acc.tools["call_1"].status, "running");

        // Same part id, updated to completed — must re-render (upsert).
        assert!(render_new_turn_parts(&mut acc, &msgs("completed", "src\n")));
        assert_eq!(acc.tools["call_1"].status, "completed");

        // No change → nothing new.
        assert!(!render_new_turn_parts(&mut acc, &msgs("completed", "src\n")));
    }

    /// The model re-sends the whole todo list on every update, each as a NEW
    /// `todowrite` call (new callID). The panel is a card-TAIL status section:
    /// no timeline row, the latest call replaces it in place, and its header
    /// carries the latest list's counts (visible while folded) and clock.
    #[test]
    fn todowrite_calls_share_one_panel_refreshed_in_place() {
        let at_a = crate::feishu::card::test_local_ms(2026, 9, 17, 10, 0);
        let at_b = crate::feishu::card::test_local_ms(2026, 9, 17, 10, 7);
        let mut acc = StreamAccumulator::new("test");
        render_parts(
            &mut acc,
            &serde_json::json!([
                { "type": "tool", "tool": "todowrite", "callID": "call_a",
                  "state": { "status": "completed", "time": { "start": at_a },
                             "input": { "todos": [ { "content": "第一步", "status": "in_progress", "priority": "high" } ] },
                             "output": "[{\"content\":\"第一步\",\"status\":\"in_progress\",\"priority\":\"high\"}]" } },
                { "type": "tool", "tool": "todowrite", "callID": "call_b",
                  "state": { "status": "completed", "time": { "start": at_b },
                             "input": { "todos": [ { "content": "第一步", "status": "completed", "priority": "high" },
                                                    { "content": "第二步", "status": "pending", "priority": "medium" } ] },
                             "output": "[{\"content\":\"第一步\",\"status\":\"completed\",\"priority\":\"high\"},{\"content\":\"第二步\",\"status\":\"pending\",\"priority\":\"medium\"}]" } },
            ]),
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
            &serde_json::json!([
                { "type": "tool", "tool": "todowrite", "callID": "call_a",
                  "state": { "status": "completed",
                             "input": { "todos": [ { "content": "第一步", "status": "in_progress" } ] },
                             "output": "[{\"content\":\"第一步\",\"status\":\"in_progress\"}]" } },
            ]),
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
            &serde_json::json!([
                { "type": "tool", "tool": "todowrite", "callID": "call_b",
                  "state": { "status": "completed",
                             "input": { "todos": [ { "content": "第一步", "status": "completed" },
                                                    { "content": "第二步", "status": "pending" } ] },
                             "output": "[{\"content\":\"第一步\",\"status\":\"completed\"},{\"content\":\"第二步\",\"status\":\"pending\"}]" } },
            ]),
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
        acc.todo_panel = Some(crate::feishu::card::tool_render::ToolPanel {
            name: "todowrite".into(),
            status: "completed".into(),
            input: None,
            output: Some(output),
        });
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

    /// A todowrite update whose state has the same byte length as the previous
    /// one (任务 A → 任务 B) must still refresh the panel: length alone is not a
    /// change signal for a re-written list.
    #[test]
    fn todowrite_same_length_update_still_refreshes() {
        let mut acc = StreamAccumulator::new("test");
        let part = |call_id: &str, task: &str| {
            serde_json::json!([
                { "type": "tool", "tool": "todowrite", "callID": call_id,
                  "state": { "status": "completed",
                             "input": { "todos": [ { "content": task, "status": "pending" } ] },
                             "output": format!("[{{\"content\":\"{task}\",\"status\":\"pending\"}}]") } }
            ])
        };
        render_parts(&mut acc, &part("call_a", "任务 A"));
        render_parts(&mut acc, &part("call_b", "任务 B"));

        let card = acc.build_card().to_string();
        assert!(card.contains("任务 B"), "latest content must render: {card}");
        assert!(
            !card.contains("任务 A"),
            "the stale list must be replaced: {card}"
        );
        assert!(acc.todo_panel.is_some(), "the tail holds the panel");
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
            &serde_json::json!([
                { "type": "reasoning", "text": "想一想" },
                { "type": "tool", "tool": "bash", "callID": "call_1",
                  "state": { "status": "completed", "output": "ok" } },
                { "type": "tool", "tool": "todowrite", "callID": "call_todo",
                  "state": { "status": "completed",
                             "input": { "todos": [ { "content": "第一步", "status": "pending" } ] },
                             "output": "[{\"content\":\"第一步\",\"status\":\"pending\"}]" } },
            ]),
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
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);

        let msgs = |reasoning: &str, text: &str| {
            vec![SessionMessage {
                info: MessageInfo {
                    id: "a1".into(),
                    role: Some("assistant".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: 100 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([
                    { "id": "prt_rsn", "type": "reasoning", "text": reasoning },
                    { "id": "prt_txt", "type": "text", "text": text },
                ]),
            }]
        };

        // Parts are written empty first, then updated with content. The empty
        // version must NOT be rendered (it would freeze the placeholder).
        assert!(!render_new_turn_parts(&mut acc, &msgs("", "")));
        assert_eq!(acc.reasoning, "");
        assert_eq!(acc.text, "");

        // Once content lands (same part ids), render it once.
        assert!(render_new_turn_parts(
            &mut acc,
            &msgs("Let me think", "Answer here")
        ));
        assert!(acc.reasoning.contains("Let me think"));
        assert!(acc.text.contains("Answer here"));

        // Re-fetching the same content must not duplicate.
        assert!(!render_new_turn_parts(
            &mut acc,
            &msgs("Let me think", "Answer here")
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
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);

        // Realistic: no "id" on the text part.
        let msgs = || {
            vec![SessionMessage {
                info: MessageInfo {
                    id: "a1".into(),
                    role: Some("assistant".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: 100 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([
                    { "type": "text", "text": "你好！很高兴认识你。" },
                    { "type": "reasoning", "text": "thinking" },
                ]),
            }]
        };

        // Poll loop renders the parts.
        assert!(render_new_turn_parts(&mut acc, &msgs()));
        assert_eq!(acc.text, "你好！很高兴认识你。");

        // Final render re-fetches the same messages — must NOT append again.
        assert!(!render_new_turn_parts(&mut acc, &msgs()));
        assert_eq!(acc.text, "你好！很高兴认识你。");
        assert_eq!(acc.reasoning, "thinking");
    }

    /// Real OpenCode failed-tool parts use `state.status: "error"` with the
    /// reason in `state.error.message` and NO `state.output` field — the panel
    /// must show the error and mark the card failed, not stay stuck "running".
    #[test]
    fn failed_tool_error_part_renders_output_and_error_state() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);

        let msgs = vec![SessionMessage {
            info: MessageInfo {
                id: "a1".into(),
                role: Some("assistant".into()),
                parent_id: None,
                time: Some(MessageTime { created: 100 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([
                { "type": "tool", "tool": "edit", "callID": "call_edit",
                  "state": {
                    "status": "error",
                    "input": { "filePath": "src/main.rs", "oldString": "a", "newString": "b" },
                    "content": [ { "type": "text", "text": "something went wrong" } ],
                    "error": { "type": "unknown", "message": "no such file" },
                    "result": null
                  } }
            ]),
        }];

        assert!(render_new_turn_parts(&mut acc, &msgs));
        let tool = acc.tools.get("call_edit").expect("tool rendered");
        assert_eq!(tool.status, "error");
        let out = tool.output.clone().unwrap_or_default();
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
    /// PLAIN STRING (`state.error: "Could not find oldString..."`), not an
    /// object — it must still show up on the panel, not vanish.
    #[test]
    fn failed_tool_string_error_renders_on_panel() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);

        let msgs = vec![SessionMessage {
            info: MessageInfo {
                id: "a1".into(),
                role: Some("assistant".into()),
                parent_id: None,
                time: Some(MessageTime { created: 100 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([
                { "type": "tool", "tool": "edit", "callID": "call_edit",
                  "state": {
                    "status": "error",
                    "input": { "filePath": "src/main.rs", "oldString": "a", "newString": "b" },
                    "error": "Could not find oldString in the file. It must match exactly."
                  } }
            ]),
        }];

        assert!(render_new_turn_parts(&mut acc, &msgs));
        let tool = acc.tools.get("call_edit").expect("tool rendered");
        assert_eq!(tool.status, "error");
        let out = tool.output.clone().unwrap_or_default();
        assert!(
            out.contains("Could not find oldString"),
            "string error message must be shown: {:?}",
            out
        );
        assert!(!out.is_empty(), "output must not be empty for a string error");
    }
    /// A completed `edit` records its real unified diff in
    /// `state.metadata.diff` (not in `state.output`, which only says "Edit
    /// applied successfully."). The panel output must carry the diff so the card
    /// shows what actually changed instead of the file name + the tool's
    /// success sentence. Failures keep their extracted error text.
    #[test]
    fn edit_part_uses_metadata_diff_as_output() {
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);

        let msgs = |status: &str, output: &str| {
            vec![SessionMessage {
                info: MessageInfo {
                    id: "a1".into(),
                    role: Some("assistant".into()),
                    parent_id: None,
                    time: Some(MessageTime { created: 100 }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{
                    "type": "tool", "tool": "edit", "callID": "call_edit",
                    "state": {
                        "status": status,
                        "input": { "filePath": "src/main.rs", "oldString": "a", "newString": "b" },
                        "output": output,
                        "metadata": {
                            "diagnostics": {},
                            "diff": "Index: src/main.rs\n======\n--- src/main.rs\n+++ src/main.rs\n@@ -1 +1 @@\n-a\n+b\n",
                            "filediff": { "file": "src/main.rs", "additions": 1, "deletions": 1 }
                        }
                    }
                }]),
            }]
        };

        // Completed: the diff replaces the generic success sentence.
        assert!(render_new_turn_parts(
            &mut acc,
            &msgs("completed", "Edit applied successfully.")
        ));
        let out = acc.tools["call_edit"].output.clone().unwrap_or_default();
        assert!(out.contains("@@ -1 +1 @@"), "diff must be shown: {}", out);
        assert!(
            !out.contains("Edit applied successfully."),
            "noise dropped: {}",
            out
        );

        // Re-rendering the same completed part is deduped (output unchanged).
        assert!(!render_new_turn_parts(
            &mut acc,
            &msgs("completed", "Edit applied successfully.")
        ));

        // A failure keeps its error text, not a diff.
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);
        let err_msgs = vec![SessionMessage {
            info: MessageInfo {
                id: "a1".into(),
                role: Some("assistant".into()),
                parent_id: None,
                time: Some(MessageTime { created: 100 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([{
                "type": "tool", "tool": "edit", "callID": "call_edit",
                "state": {
                    "status": "error",
                    "input": { "filePath": "src/main.rs", "oldString": "a", "newString": "b" },
                    "error": { "type": "unknown", "message": "no such file" },
                    "metadata": { "diff": "Index: src/main.rs\n@@ -1 +1 @@\n-a\n+b\n" }
                }
            }]),
        }];
        assert!(render_new_turn_parts(&mut acc, &err_msgs));
        let out = acc.tools["call_edit"].output.clone().unwrap_or_default();
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
        use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.turn_started_ms = Some(epoch);

        let msgs = vec![SessionMessage {
            info: MessageInfo {
                id: "a1".into(),
                role: Some("assistant".into()),
                parent_id: None,
                time: Some(MessageTime { created: 100 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: serde_json::json!([
                { "type": "reasoning", "text": "Let me check" },
                { "type": "text", "text": "The answer." },
                { "type": "tool", "tool": "bash", "callID": "call_1",
                  "state": { "status": "completed", "input": { "command": "ls" }, "output": "src" } },
            ]),
        }];

        // Long turn: the poll loop already rendered the parts.
        assert!(render_new_turn_parts(&mut acc, &msgs));

        // Final reconcile: nothing new from messages → falls back to the
        // response parts (identical content). Must NOT append again.
        let resp_parts = serde_json::json!([
            { "type": "reasoning", "text": "Let me check" },
            { "type": "text", "text": "The answer." },
            { "type": "tool", "tool": "bash", "callID": "call_1",
              "state": { "status": "completed", "input": { "command": "ls" }, "output": "src" } },
        ]);
        assert!(!render_parts(&mut acc, &resp_parts));
        assert_eq!(acc.text, "The answer.");
        assert_eq!(acc.reasoning, "Let me check");
        assert_eq!(acc.tools["call_1"].output.as_deref(), Some("src"));
    }
}
