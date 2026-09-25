//! The Turn's render internals (spec #298, A2b).
//!
//! The render poll loop, the part rendering, and the session subtitle/title
//! refresh live here, behind the Turn's interface (`Turn::session_subtitle` /
//! `Turn::render_and_flush` in the parent module) and its private
//! [`RenderPoll`] task. Nothing here is reachable from outside the Turn module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::Instrument;

use crate::backend::{MessageRole, Part, SessionTranscript, ToolStatus};
use crate::bridge::core::SESSION_INFO_TIMEOUT;
use crate::bridge::handles::{CardsHandle, SessionsHandle, TurnHandles};
use crate::bridge::span;
use crate::bridge::turn::state::{RenderedPart, StreamAccumulator};
use crate::config::ThreadKey;

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

/// Render one typed part into the accumulator, applying the dedup rules: text
/// and reasoning are tracked by their content (OpenCode part payloads carry NO
/// `id`, AGENTS.md #9), and a tool call re-renders exactly when its typed panel
/// revision changed. Returns true when the part rendered (not skipped as
/// duplicate/empty).
fn render_part(acc: &mut StreamAccumulator, part: &Part) -> bool {
    match part {
        // Reasoning/text parts are written with empty text first, then updated
        // with the full content. Only render once they have content, otherwise
        // we'd freeze the placeholder version.
        Part::Text(text) => {
            if text.text.is_empty() {
                return false;
            }
            if !acc.rendered_parts.insert(RenderedPart::Text(text.text.clone())) {
                return false;
            }
            acc.push_text_at(text.started_at, &text.text);
            acc.card_state = crate::feishu::card::CardState::Streaming;
        }
        Part::Reasoning(reasoning) => {
            if reasoning.text.is_empty() {
                return false;
            }
            if !acc
                .rendered_parts
                .insert(RenderedPart::Reasoning(reasoning.text.clone()))
            {
                return false;
            }
            acc.push_reasoning_at(reasoning.started_at, &reasoning.text);
            acc.card_state = crate::feishu::card::CardState::Reasoning;
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
            let already_rendered = if call.identity.name == "todowrite" {
                acc.todo_panel.as_ref().is_some_and(|panel| panel.call() == call)
                    && acc.todo_shown_at == call.started_at
            } else {
                acc.tools
                    .get(&call.identity.call_id)
                    .is_some_and(|panel| panel.call() == call)
            };
            if already_rendered {
                return false;
            }
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
        // Step boundaries, patches and part kinds this build does not model
        // render nothing — there is no content to add or dedupe.
        Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::Other(_) => return false,
    }
    // card_state / running-tool changes reset the header phase timer.
    acc.refresh_phase();
    true
}

/// Render a batch of typed parts into the accumulator, skipping anything
/// already rendered (same dedup as the poll loop). Returns true if anything
/// new was rendered. Used as the final fallback when the incremental poll
/// missed parts.
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
    let Some(message) = transcript
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User && message.id.as_str() == cola_message_id)
    else {
        return;
    };
    // `anchor()` keeps identity and server time together; a message with no
    // server time cannot anchor (and is retried on the next poll).
    acc.turn_anchor = message.anchor();
}

/// Render the parts of this turn's assistant messages that haven't been
/// rendered yet. Returns true if anything new was rendered.
///
/// Turn membership is the Session Transcript's shared `turn_for_user`
/// projection, anchored on the Turn's own anchor — the SERVER's message
/// identity and time for this turn (#190). Filtering against cola's submit
/// clock instead dropped the new turn's parts when the server ran behind cola,
/// and bled the previous turn's parts into the new card when it ran ahead.
/// Until the anchor is observed nothing renders: with two skewed clocks there
/// is no threshold that tells the two turns apart.
///
/// A message still in flight (no server completion stamp) is always rendered,
/// whatever its created time: the previous run may still be streaming when this
/// turn's user message lands, and that tail is live content the card must show
/// (#310). A completed message belongs to this turn when it finished at/after
/// the anchor — either created within the turn or still being produced as the
/// turn began — while one that finished before the anchor stays the previous
/// turn's and never bleeds in (#190).
pub(super) fn render_new_turn_parts(acc: &mut StreamAccumulator, transcript: &SessionTranscript) -> bool {
    capture_turn_anchor(acc, transcript);
    let Some(anchor) = acc.turn_anchor.clone() else {
        return false;
    };
    let mut rendered_any = false;
    for message in transcript.turn_for_user(&anchor).messages {
        // Capture the answering model + token usage for the card footer.
        if let Some(model) = &message.model {
            acc.model_id = Some(model.model_id.clone());
            // The decoder reports an absent provider as an empty string; an
            // empty value leaves the last known provider in place.
            if !model.provider_id.is_empty() {
                acc.provider_id = Some(model.provider_id.clone());
            }
        }
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
            if render_part(acc, part) {
                rendered_any = true;
            }
        }
    }
    rendered_any
}

/// Render the session's transcript into the streaming card and flush it when
/// something changed. The shared heart of both render loops — `render_poll_loop`
/// (cola's own prompts) and the external-message renderer
/// (`bridge::external`) — so the two never drift apart.
///
/// Returns `Some((new_parts, text_len, reasoning_len))` when the accumulator is
/// still present (the statistics are for logging); `None` when it vanished (the
/// caller should stop).
pub(super) async fn render_and_flush(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    transcript: &SessionTranscript,
) -> Option<(usize, usize, usize)> {
    // OpenCode auto-renames sessions after a turn; follow the server's live
    // title so the card subtitle doesn't stay on the "new session" default.
    refresh_session_title(cards, sessions, backend, session_id).await;
    let (changed, header_changed, new_parts, text_len, reasoning_len) = {
        let mut live = cards.cards.lock().await;
        let card = live.get_mut(session_id)?;
        let before = card.acc.rendered_parts.len();
        let changed = render_new_turn_parts(&mut card.acc, transcript);
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
    if changed || header_changed || context_changed {
        Turn::flush_card(cards, session_id).await;
    }
    Some((new_parts, text_len, reasoning_len))
}

/// Incremental renderer: while the synchronous prompt is in flight, poll the
/// session's transcript and flush the card as parts complete (reasoning, tools,
/// text). `done` stops the loop once the prompt returns. `poll_ms` is the
/// injected cadence (`TurnConfig::turn_render_poll_ms`), so tests never wait
/// on the production 1.5 s.
async fn render_poll_loop(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
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
        match render_and_flush(cards, sessions, backend, &session_id, &transcript).await {
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
                render_poll_loop(&cards, &sessions, &backend, sid, flag, poll_ms).await;
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
        MessageId, MessageTime, ReasoningPart, StepFinish, StepStart, ToolCall, ToolIdentity, ToolOutput,
        TranscriptMessage, TurnAnchor,
    };
    use crate::bridge::App;
    use crate::bridge::test_support::{
        MockBackend, PlatformCall, RecordingPlatform, build_app, realistic_parts, seed_cover_title,
        seed_entry, test_config, test_work_dir, text_part, turn_anchor, typed_message,
    };
    use crate::bridge::turn::state::StreamAccumulator;
    use crate::feishu::card::CardState;

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
        let _ = render_and_flush(&cards, &app.sessions_handle(), &app.opencode, sid, &transcript).await;
        assert_eq!(
            platform.updated_cards().await.len(),
            1,
            "the seeded header must flush once"
        );

        // The header signature moves (the state label flips) with no new parts.
        Turn::set_card_state(&cards, sid, CardState::Reasoning).await;
        let _ = render_and_flush(&cards, &app.sessions_handle(), &app.opencode, sid, &transcript).await;
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
