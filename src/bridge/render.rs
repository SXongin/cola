use std::sync::Arc;

use crate::bridge::handler::App;
use crate::bridge::streaming::StreamAccumulator;

/// Render canonical message parts (from `POST /session/{id}/message` response)
/// into the accumulator so the card shows the assistant's final result.
fn render_part(acc: &mut StreamAccumulator, part: &serde_json::Value) {
    match part.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.text.push_str(t);
            }
            acc.card_state = crate::feishu::card::CardState::Streaming;
        }
        Some("reasoning") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.reasoning.push_str(t);
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
            let input = part
                .get("state")
                .and_then(|s| s.get("input"))
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let output = part
                .get("state")
                .and_then(|s| s.get("output"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    part.get("state")
                        .and_then(|s| s.get("metadata"))
                        .and_then(|m| m.get("output"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });
            acc.tools.insert(
                call_id,
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

pub(crate) fn render_parts(acc: &mut StreamAccumulator, parts: &serde_json::Value) {
    let Some(arr) = parts.as_array() else { return };
    for part in arr {
        render_part(acc, part);
    }
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
        let Some(parts) = m.parts.as_array() else { continue };
        for part in parts {
            let ptype = part
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            // Reasoning/text parts are written with empty text first, then
            // updated with the full content. Only render once they have content,
            // otherwise we'd freeze the placeholder version.
            if ptype == "reasoning" || ptype == "text" {
                let Some(t) = part.get("text").and_then(|v| v.as_str()) else {
                    continue;
                };
                if t.is_empty() {
                    continue;
                }
                // OpenCode part payloads carry NO `id` (the DB column id is not
                // serialised), so dedupe on content: the same part re-fetched
                // (poll + final render) must not append twice. This is what
                // previously doubled the card text (81 → 162 chars).
                let dedup_key = format!("{}:{}", ptype, t);
                if acc.rendered_parts.contains(&dedup_key) {
                    continue;
                }
                acc.rendered_parts.insert(dedup_key);
                render_part(acc, part);
                rendered_any = true;
                continue;
            }
            // Tool parts get updated in place (running → completed); re-render
            // whenever the state signature changes so panels don't stay stuck
            // on "running".
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
                if let Some(prev) = acc.rendered_tool_states.get(call_id) {
                    if prev == &sig {
                        continue;
                    }
                }
                acc.rendered_tool_states.insert(call_id.to_string(), sig);
                render_part(acc, part);
                rendered_any = true;
                continue;
            }
            // Everything else (step-start/step-finish/patch): render once.
            let part_id = part.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            if let Some(id) = &part_id {
                if acc.rendered_parts.contains(id) {
                    continue;
                }
                acc.rendered_parts.insert(id.clone());
            }
            render_part(acc, part);
            rendered_any = true;
        }
    }
    rendered_any
}

/// Push the accumulator's current card to Feishu as an update of the loading
/// card, so the user sees reasoning/tool/text appear incrementally.
pub(crate) async fn flush_card(app: &Arc<App>, session_id: &str) {
    let accs = app.accumulators.lock().await;
    let Some(acc) = accs.get(session_id) else { return };
    let card = acc.build_card();
    drop(accs);
    let card_id = {
        let ids = app.card_message_ids.lock().await;
        ids.get(session_id).cloned()
    };
    if let Some(msg_id) = card_id
        && let Err(e) = app.feishu.update_message(&msg_id, &card).await
    {
        tracing::warn!("Card update failed: {}", e);
    }
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
        let (changed, parts_rendered, text_len, reasoning_len) = {
            let mut accs = app.accumulators.lock().await;
            let Some(acc) = accs.get_mut(&session_id) else {
                continue;
            };
            let before = acc.rendered_parts.len();
            let changed = render_new_turn_parts(acc, &msgs, epoch_ms);
            (
                changed,
                acc.rendered_parts.len() - before,
                acc.text.len(),
                acc.reasoning.len(),
            )
        };
        if changed {
            tracing::info!(
                "render poll: {} new parts, text={} reasoning={}",
                parts_rendered,
                text_len,
                reasoning_len
            );
            flush_card(app, &session_id).await;
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
        assert!(tool.input.as_deref().unwrap().contains("pwd"));

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
}
