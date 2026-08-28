use std::sync::Arc;

use crate::bridge::handler::App;
use crate::bridge::streaming::StreamAccumulator;

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

/// Render canonical message parts (from `POST /session/{id}/message` response)
/// into the accumulator so the card shows the assistant's final result.
fn render_part(acc: &mut StreamAccumulator, part: &serde_json::Value) {
    match part.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.push_text(t);
            }
            acc.card_state = crate::feishu::card::CardState::Streaming;
        }
        Some("reasoning") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.push_reasoning(t);
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
            // field on tool parts — reading it silently lost every result.
            let output = extract_tool_output(part, status);
            acc.push_tool(
                &call_id,
                crate::feishu::card::ToolPanel {
                    name: name.to_string(),
                    status: status.to_string(),
                    input,
                    output,
                },
            );
            if status == "running" {
                acc.card_state = crate::feishu::card::CardState::Streaming;
            }
        }
        Some("step-start") | Some("step-finish") | Some("patch") => {
            // No visible content for these
        }
        _ => {}
    }
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
        let sig = format!("{}|{}", status, output_len);
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

/// Render the parts of this turn's assistant messages that haven't been
/// rendered yet. Returns true if anything new was rendered.
pub(crate) fn render_new_turn_parts(
    acc: &mut StreamAccumulator,
    msgs: &[crate::opencode::client::SessionMessage],
    epoch_ms: i64,
) -> bool {
    let mut rendered_any = false;
    for m in msgs {
        let is_assistant = m.info.role.as_deref() == Some("assistant");
        let in_turn = m
            .info
            .time
            .as_ref()
            .map(|t| t.created >= epoch_ms)
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

pub(crate) async fn flush_card(app: &Arc<App>, session_id: &str) {
    for _ in 0..MAX_CARD_CHAIN {
        let (card, full) = {
            let mut accs = app.accumulators.lock().await;
            match accs.get_mut(session_id) {
                Some(acc) => acc.build_card_with_split(),
                None => return,
            }
        };
        let card_id = {
            let ids = app.card_message_ids.lock().await;
            ids.get(session_id).cloned()
        };
        let Some(card_id) = card_id else { return };
        if !full {
            if let Err(e) = app.feishu.update_message(&card_id, &card).await {
                tracing::warn!("Card update failed: {}", e);
            }
            return;
        }

        // The card hit the component limit: finalize it with a "to be
        // continued" marker, then send a FRESH continuation card that takes over
        // subsequent updates. `build_card_with_split` already advanced
        // `render_from` past the split point, so the continuation holds only the
        // remaining content — nothing is lost.
        if let Err(e) = app.feishu.update_message(&card_id, &card).await {
            tracing::warn!("Card split update failed: {}", e);
        }
        let reply_to = {
            let accs = app.accumulators.lock().await;
            accs.get(session_id).and_then(|a| a.reply_to_message_id.clone())
        };
        let Some(reply_to) = reply_to else { return };
        let (cont_card, cont_full) = {
            let mut accs = app.accumulators.lock().await;
            accs.get_mut(session_id)
                .map(|acc| acc.build_card_with_split())
                .unwrap_or((serde_json::json!({}), false))
        };
        match app.feishu.reply_card(&reply_to, &cont_card).await {
            Ok(new_id) => {
                app.card_message_ids
                    .lock()
                    .await
                    .insert(session_id.to_string(), new_id);
                if !cont_full {
                    return;
                }
                // The continuation is itself over the limit → loop and split again.
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
    app: &Arc<App>,
    session_id: &str,
    epoch_ms: i64,
    msgs: &[crate::opencode::client::SessionMessage],
) -> Option<(usize, usize, usize)> {
    // OpenCode auto-renames sessions after a turn; follow the server's live
    // title so the card subtitle doesn't stay on the "new session" default.
    app.refresh_session_title(session_id).await;
    let (changed, new_parts, text_len, reasoning_len) = {
        let mut accs = app.accumulators.lock().await;
        let acc = accs.get_mut(session_id)?;
        let before = acc.rendered_parts.len();
        let changed = render_new_turn_parts(acc, msgs, epoch_ms);
        (
            changed,
            acc.rendered_parts.len() - before,
            acc.text.len(),
            acc.reasoning.len(),
        )
    };
    if changed {
        flush_card(app, session_id).await;
    }
    Some((new_parts, text_len, reasoning_len))
}

/// Incremental renderer: while the synchronous prompt is in flight, poll the
/// session's messages and flush the card as parts complete (reasoning, tools,
/// text). `done` stops the loop once the prompt returns.
pub(crate) async fn render_poll_loop(
    app: &Arc<App>,
    session_id: String,
    epoch_ms: i64,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
        if done.load(Ordering::SeqCst) {
            return;
        }
        let msgs = match app.opencode.messages(&session_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("render poll messages: {}", e);
                continue;
            }
        };
        match render_and_flush(app, &session_id, epoch_ms, &msgs).await {
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
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 1000;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

        let msgs = vec![
            // Old turn assistant message (before epoch) — skipped.
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

        assert!(render_new_turn_parts(&mut acc, &msgs, epoch));
        assert!(acc.reasoning.contains("Let me think"));
        assert_eq!(acc.tools.len(), 1);
        assert_eq!(acc.rendered_parts.len(), 1);
        assert_eq!(acc.rendered_tool_states.len(), 1);
        assert!(!acc.text.contains("question"));
        assert!(!acc.reasoning.contains("old reasoning"));

        assert!(!render_new_turn_parts(&mut acc, &msgs, epoch));
    }

    #[test]
    fn tool_part_update_re_renders_panel() {
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

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
        assert!(render_new_turn_parts(&mut acc, &msgs("running", ""), epoch));
        assert_eq!(acc.tools["call_1"].status, "running");

        // Same part id, updated to completed — must re-render (upsert).
        assert!(render_new_turn_parts(
            &mut acc,
            &msgs("completed", "src\n"),
            epoch
        ));
        assert_eq!(acc.tools["call_1"].status, "completed");

        // No change → nothing new.
        assert!(!render_new_turn_parts(
            &mut acc,
            &msgs("completed", "src\n"),
            epoch
        ));
    }

    #[test]
    fn empty_then_updated_part_renders_once_with_content() {
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

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
        assert!(!render_new_turn_parts(&mut acc, &msgs("", ""), epoch));
        assert_eq!(acc.reasoning, "");
        assert_eq!(acc.text, "");

        // Once content lands (same part ids), render it once.
        assert!(render_new_turn_parts(
            &mut acc,
            &msgs("Let me think", "Answer here"),
            epoch
        ));
        assert!(acc.reasoning.contains("Let me think"));
        assert!(acc.text.contains("Answer here"));

        // Re-fetching the same content must not duplicate.
        assert!(!render_new_turn_parts(
            &mut acc,
            &msgs("Let me think", "Answer here"),
            epoch
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
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

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
        assert!(render_new_turn_parts(&mut acc, &msgs(), epoch));
        assert_eq!(acc.text, "你好！很高兴认识你。");

        // Final render re-fetches the same messages — must NOT append again.
        assert!(!render_new_turn_parts(&mut acc, &msgs(), epoch));
        assert_eq!(acc.text, "你好！很高兴认识你。");
        assert_eq!(acc.reasoning, "thinking");
    }

    /// Real OpenCode failed-tool parts use `state.status: "error"` with the
    /// reason in `state.error.message` and NO `state.output` field — the panel
    /// must show the error and mark the card failed, not stay stuck "running".
    #[test]
    fn failed_tool_error_part_renders_output_and_error_state() {
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

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

        assert!(render_new_turn_parts(&mut acc, &msgs, epoch));
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
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

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

        assert!(render_new_turn_parts(&mut acc, &msgs, epoch));
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
    /// Regression: the final reconcile falls back to `render_parts(resp.parts)`
    /// when the incremental poll already rendered everything (`render_new_turn_parts`
    /// returns false). `render_parts` used to append unconditionally, doubling the
    /// card text on long turns (the poll renders the answer while the card is still
    /// "streaming", then the final card repeats it).
    #[test]
    fn render_parts_fallback_does_not_double_already_rendered_text() {
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

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
        assert!(render_new_turn_parts(&mut acc, &msgs, epoch));

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
