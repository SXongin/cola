use crate::bridge::EventSink;
use crate::feishu::Platform;
use crate::feishu::event::MessageReceiveEvent;
use crate::feishu::message::{extract_image_keys, is_mentioned, parse_message_content, strip_mentions};
#[cfg(test)]
use crate::feishu::pbbp2::Routing;
use crate::feishu::pbbp2::{self, Frame};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const MAX_RECONNECT_DELAY_SECS: u64 = 30;

/// Bounded event-id dedupe for Feishu's at-least-once delivery. Capped so the
/// set cannot grow without bound; on overflow the whole set is cleared — the
/// 5-minute age filter makes stale ids worthless anyway, and a cleared window
/// only means a handful of genuinely duplicate events slip through to the age
/// filter (which skips anything old).
pub struct DedupeSet {
    events: HashSet<String>,
    cap: usize,
}

impl DedupeSet {
    pub fn new(cap: usize) -> Self {
        Self {
            events: HashSet::new(),
            cap,
        }
    }

    /// Insert `id`, returning true when it was already present (a re-delivery
    /// that must not be dispatched again).
    pub fn check_and_insert(&mut self, id: &str) -> bool {
        if self.events.contains(id) {
            return true;
        }
        if self.events.len() >= self.cap {
            self.events.clear();
        }
        self.events.insert(id.to_string());
        false
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.events.len()
    }
}

/// State the ws platform owns, folded out of the bridge core: event-id dedupe
/// (Feishu re-delivers at-least-once) and cola's own open_id (needed to strip
/// the bot's own @mention from prompt text).
pub struct WsState {
    seen_event_ids: Mutex<DedupeSet>,
    bot_open_id: Mutex<Option<String>>,
}

impl WsState {
    pub fn new() -> Self {
        Self {
            seen_event_ids: Mutex::new(DedupeSet::new(10_000)),
            bot_open_id: Mutex::new(None),
        }
    }

    /// cola's own Feishu open_id, fetched lazily once so @mentions of the bot
    /// can be recognised. `Ok(None)` means mention handling is disabled.
    async fn bot_open_id(&self, feishu: &Arc<dyn Platform>) -> Option<String> {
        {
            let cached = self.bot_open_id.lock().await;
            if cached.is_some() {
                return cached.clone();
            }
        }
        match feishu.bot_open_id().await {
            Ok(id) => {
                tracing::info!("bot open_id: {}", id);
                *self.bot_open_id.lock().await = Some(id.clone());
                Some(id)
            }
            Err(e) => {
                tracing::warn!("failed to fetch bot open_id, mention handling disabled: {}", e);
                None
            }
        }
    }
}

impl Default for WsState {
    fn default() -> Self {
        Self::new()
    }
}

/// The reply headers for an event response: echo the request's headers and
/// append `biz_rt` (processing time ms), matching the Lark SDK.
fn reply_headers(request: &Frame) -> Vec<(&str, &str)> {
    let mut headers: Vec<(&str, &str)> = request
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    headers.push(("biz_rt", "1"));
    headers
}

/// Build a response frame for a received card action. Echoes the request
/// frame's fields (seq_id, log_id, service, method, headers, payload_*)
/// and sets the payload to the ack JSON, matching the Lark SDK's Response
/// format: {"code":200,"headers":null,"data":"<base64>"}.
fn build_response_frame(
    request: &Frame,
    result: Option<&crate::bridge::handler::CardActionResult>,
) -> Vec<u8> {
    use base64::Engine;

    // CardActionTriggerResponse: optionally show a Toast and/or update the card
    // with the result. Format: {"toast":{...},"card":{"type":"raw","data":{card json}}}
    let rsp_json = if let Some(result) = result {
        let mut obj = serde_json::json!({});
        if let Some(toast) = &result.toast {
            obj["toast"] = serde_json::json!({ "type": "success", "content": toast });
        }
        if let Some(card) = &result.card {
            obj["card"] = serde_json::json!({ "type": "raw", "data": card });
        }
        obj.to_string()
    } else {
        "{}".to_string()
    };
    // Response: {"code":200,"headers":null,"data":"<base64 of rsp>"}
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(rsp_json.as_bytes());
    let payload = format!(r#"{{"code":200,"headers":null,"data":"{}"}}"#, data_b64);

    pbbp2::encode(&request.routing, &reply_headers(request), payload.as_bytes())
}

/// Build the ack frame Feishu requires for EVERY ordinary event (message
/// receive, bot added, ...). The Lark SDK sends this after handling each
/// `MessageTypeEvent`; without it Feishu re-delivers the event forever and
/// eventually stops pushing new ones. Same routing echo as the card response,
/// but the payload data is null (no card to update).
fn build_event_ack_frame(request: &Frame) -> Vec<u8> {
    pbbp2::encode(
        &request.routing,
        &reply_headers(request),
        br#"{"code":200,"headers":null,"data":null}"#,
    )
}

// --- WebSocket event loop ---

pub async fn event_loop(
    sink: &Arc<dyn EventSink>,
    feishu: &Arc<dyn Platform>,
    state: &Arc<WsState>,
) -> crate::error::Result<()> {
    let mut attempt = 0;
    loop {
        match connect_and_listen(sink, feishu, state).await {
            Ok(()) => tracing::info!("WebSocket closed cleanly, reconnecting..."),
            Err(e) => tracing::warn!("WebSocket error: {}, reconnecting...", e),
        }
        let delay = (2u64.pow(attempt.min(5))).min(MAX_RECONNECT_DELAY_SECS);
        tracing::info!("Reconnecting in {}s (attempt {})", delay, attempt + 1);
        sleep(Duration::from_secs(delay)).await;
        attempt += 1;
    }
}

async fn connect_and_listen(
    sink: &Arc<dyn EventSink>,
    feishu: &Arc<dyn Platform>,
    state: &Arc<WsState>,
) -> crate::error::Result<()> {
    let ws_url = feishu.get_ws_endpoint().await?;
    tracing::info!("WS endpoint resolved");
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .map_err(|e| crate::error::BridgeError::Feishu(format!("WS connect failed: {}", e)))?;
    tracing::info!("Connected to Feishu WebSocket");
    handle_connection(ws_stream, sink, feishu, state).await
}

async fn handle_connection(
    mut ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    sink: &Arc<dyn EventSink>,
    feishu: &Arc<dyn Platform>,
    state: &Arc<WsState>,
) -> crate::error::Result<()> {
    // Proactive keepalive: send a control ping periodically (mirrors the Lark
    // SDK's pingLoop). Combined with the read timeout below, an idle or half-
    // dead connection is detected and torn down so event_loop reconnects.
    const PING_INTERVAL: Duration = Duration::from_secs(60);
    const READ_TIMEOUT: Duration = Duration::from_secs(180);

    let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
    // First tick fires immediately; skip it so we don't ping before the read loop starts.
    ping_ticker.tick().await;

    loop {
        tokio::select! {
            _ = ping_ticker.tick() => {
                if let Err(e) = ws
                    .send(tokio_tungstenite::tungstenite::Message::Binary(pbbp2::ping().into()))
                    .await
                {
                    tracing::warn!("WS keepalive ping failed: {}", e);
                    return Err(crate::error::BridgeError::Feishu(format!(
                        "WS keepalive ping failed: {}", e
                    )));
                }
            }
            msg = tokio::time::timeout(READ_TIMEOUT, ws.next()) => {
                match msg {
                    Err(_) => {
                        // No frame at all for READ_TIMEOUT — the connection is
                        // half-dead (TCP still ESTAB but server silent). Tear it
                        // down so event_loop reconnects; otherwise cola wedges
                        // forever and stops receiving new messages.
                        tracing::warn!("WS read timeout after {}s, reconnecting", READ_TIMEOUT.as_secs());
                        return Err(crate::error::BridgeError::Feishu(
                            format!("WS read timeout after {}s", READ_TIMEOUT.as_secs()),
                        ));
                    }
                    Ok(None) => {
                        tracing::info!("WebSocket closed by server");
                        return Ok(());
                    }
                    Ok(Some(msg)) => match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Binary(data)) => {
                            handle_binary_frame(&data, &mut ws, sink, feishu, state).await?;
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Ping(data)) => {
                            ws.send(tokio_tungstenite::tungstenite::Message::Pong(data))
                                .await
                                .map_err(|e| crate::error::BridgeError::Feishu(format!("Pong failed: {}", e)))?;
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                            tracing::info!("WebSocket closed by server");
                            return Ok(());
                        }
                        Err(e) => {
                            return Err(crate::error::BridgeError::Feishu(format!(
                                "WebSocket error: {}",
                                e
                            )));
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

/// Ack an ordinary WS event frame (message receive, bot added, ...). Feishu's
/// long-connection protocol is at-least-once: an unacked event is re-delivered
/// forever, and a client that never acks is eventually treated as dead. Same
/// routing echo as the card response, but the payload data is null.
async fn send_event_ack(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, frame: &Frame) {
    let ack_bytes = build_event_ack_frame(frame);
    if let Err(e) = ws
        .send(tokio_tungstenite::tungstenite::Message::Binary(ack_bytes.into()))
        .await
    {
        tracing::warn!("WS event ack send failed: {}", e);
    }
}

/// Extract the `event.action.value` from a `card.action.trigger` payload, and
/// normalize the two non-button component shapes into the standard value cola's
/// handler expects:
/// - folding button groups / dropdowns deliver the chosen option as a string
///   (`action.option`, encoded "qi|label" by the card) → decoded into
///   `question_index` + `answer`;
/// - form containers deliver their inputs as `action.form_value` → the first
///   non-empty input value becomes `answer` (the typed custom answer).
fn extract_card_action_value(payload: &[u8]) -> Option<serde_json::Value> {
    let v = serde_json::from_slice::<serde_json::Value>(payload).ok()?;
    let a = v.get("event")?.get("action")?;
    let mut val = match a.get("value").cloned() {
        Some(value) => value,
        None => {
            let name = a.get("name").and_then(|n| n.as_str())?;
            let parts: Vec<&str> = name.split('|').collect();
            if parts.len() >= 3 && parts[0] == "switchsearch" {
                // `/switch` card search form: the button carries the routing
                // ("switchsearch|<chat>|<thread>|<scope>"), the typed keyword
                // arrives as `form_value.search` below.
                serde_json::json!({
                    "action": "switch",
                    "op": "search",
                    "chat_id": parts[1],
                    "thread_id": parts[2],
                    "scope": parts.get(3).copied().unwrap_or(""),
                })
            } else if parts[0] == "submitm" && (parts.len() == 4 || parts.len() == 5) {
                // A multi-select custom-answer form submit: the routing payload
                // is encoded in the name ("submitm|<req>|<ses>|<qi>[|<dir>]")
                // and `reply` is "custom" — the handler ADDS the typed label to
                // the toggled set instead of treating it as an option toggle.
                let mut val = serde_json::json!({
                    "action": "question",
                    "reply": "custom",
                    "request_id": parts[1],
                    "session_id": parts[2],
                    "question_index": parts[3].parse::<u64>().ok()?,
                });
                if let Some(dir) = parts.get(4) {
                    val["directory"] = serde_json::Value::String(dir.to_string());
                }
                val
            } else if parts[0] != "submit" || !(parts.len() == 4 || parts.len() == 5) {
                // Form submit callbacks don't always deliver the button `value`;
                // the routing payload is encoded in the button `name`
                // ("submit|<req>|<ses>|<qi>[|<dir>]") — rebuild the value from it.
                // The trailing directory is legacy (dropped so the name stays
                // under Feishu's 100-char limit); the handler re-resolves it.
                return None;
            } else {
                let mut val = serde_json::json!({
                    "action": "question",
                    "reply": "answer",
                    "request_id": parts[1],
                    "session_id": parts[2],
                    "question_index": parts[3].parse::<u64>().ok()?,
                });
                if let Some(dir) = parts.get(4) {
                    val["directory"] = serde_json::Value::String(dir.to_string());
                }
                val
            }
        }
    };
    if let Some(opt) = a.get("option").and_then(|o| o.as_str()) {
        // "qi|label" from an overflow/select option.
        if let Some((qi, label)) = opt.split_once('|') {
            if let Ok(qi_num) = qi.parse::<u64>() {
                val["question_index"] = serde_json::Value::from(qi_num);
            }
            val["answer"] = serde_json::Value::String(label.to_string());
        }
    }
    if let Some(fv) = a.get("form_value").and_then(|f| f.as_object()) {
        // Typed custom answer: first non-empty input value.
        for (_, v) in fv {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                val["answer"] = serde_json::Value::String(s.to_string());
                break;
            }
        }
        // `/switch` card search: the typed keyword from the search input.
        if val.get("action").and_then(|v| v.as_str()) == Some("switch")
            && val.get("op").and_then(|v| v.as_str()) == Some("search")
            && let Some(s) = fv.get("search").and_then(|v| v.as_str())
        {
            val["keyword"] = serde_json::Value::String(s.to_string());
        }
    }
    // The card's own message id. In the schema 2.0 callback it lives on
    // `event.context.open_message_id`; older shapes carried it on the action
    // object. `/topic --adopt`'s card button needs it to `reply_in_thread` off
    // the card and create the topic (ADR-0016); without it there is no message
    // inside the new topic to anchor fallback cards on.
    let open_message_id = v
        .get("event")
        .and_then(|e| e.get("context"))
        .and_then(|c| c.get("open_message_id"))
        .and_then(|m| m.as_str())
        .or_else(|| a.get("open_message_id").and_then(|m| m.as_str()));
    if let Some(open_message_id) = open_message_id {
        val["open_message_id"] = serde_json::Value::String(open_message_id.to_string());
    }
    Some(val)
}

/// What a WS frame needs from the connection handler, decided purely (no socket
/// I/O). The handler maps each outcome to the actual writes/reads.
pub enum FrameAction {
    /// Answer the server's heartbeat ping with a pong.
    Pong,
    /// Send the plain event ack (deduped / stale / unparseable / non-message
    /// event) — Feishu re-delivers unacked events forever, so even a skipped
    /// event must be acked.
    Ack,
    /// Ack, then dispatch a `im.message.receive_v1` to the bridge.
    Message(Box<MessageReceiveEvent>),
    /// Dispatch a `card.action.trigger`; the ack's payload carries the sink
    /// result (built by the caller, which owns the socket).
    CardAction(serde_json::Value),
    /// Nothing to send (control/card/unknown frame types).
    None,
}

/// The pure per-frame decision: dedupe by event id, the 5-minute age filter,
/// type dispatch, a single typed parse, and the what-to-ack / what-to-dispatch
/// call. Everything except the socket reads/writes.
///
/// `seen` is the (bounded) dedupe set; `bot_open_id` lets the message parse
/// strip the bot's own @mention placeholder.
fn process_frame(frame: &Frame, seen: &mut DedupeSet) -> FrameAction {
    let msg_type = frame.headers.get("type").map(|s| s.as_str()).unwrap_or("unknown");
    match msg_type {
        "ping" => FrameAction::Pong,
        "event" => {
            let payload = &frame.payload;
            // Single typed parse of the event header: type routing first.
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
                // Unparseable event — still ack, so it stops being re-delivered.
                return FrameAction::Ack;
            };
            let event_type = v
                .get("header")
                .and_then(|h| h.get("event_type"))
                .and_then(|t| t.as_str())
                .unwrap_or_default();

            if event_type == "im.message.receive_v1" {
                // Dedup by event_id from the JSON payload (stable across retries).
                let event_id = v
                    .get("header")
                    .and_then(|h| h.get("event_id"))
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string());
                if let Some(ref eid) = event_id
                    && seen.check_and_insert(eid)
                {
                    tracing::info!("Deduped event_id={}", &eid[..eid.len().min(30)]);
                    return FrameAction::Ack;
                }
                // Skip events older than 5 minutes (Feishu replays old unacked
                // events after an outage).
                let now_ms = chrono::Utc::now().timestamp_millis();
                let event_age = v
                    .get("header")
                    .and_then(|h| h.get("create_time"))
                    .and_then(|c| c.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|ct| now_ms - ct);
                if let Some(age_ms) = event_age
                    && age_ms > 300_000
                {
                    tracing::info!("Skipping old event (age={}s)", age_ms / 1000);
                    return FrameAction::Ack;
                }
                // One typed parse — never re-parse the raw payload later.
                match serde_json::from_slice::<MessageReceiveEvent>(payload) {
                    Ok(event) => FrameAction::Message(Box::new(event)),
                    Err(_) => FrameAction::Ack,
                }
            } else if event_type == "card.action.trigger" {
                match extract_card_action_value(payload) {
                    Some(value) => FrameAction::CardAction(value),
                    None => FrameAction::Ack,
                }
            } else {
                // Any other event type (bot added, message recalled, ...) still
                // needs an ack, otherwise Feishu re-delivers it forever.
                FrameAction::Ack
            }
        }
        _ => FrameAction::None,
    }
}

async fn handle_binary_frame(
    data: &[u8],
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    sink: &Arc<dyn EventSink>,
    feishu: &Arc<dyn Platform>,
    state: &Arc<WsState>,
) -> crate::error::Result<()> {
    let frame = match Frame::decode(data) {
        Some(v) => v,
        None => {
            tracing::warn!("Failed to parse WS binary frame (len={})", data.len());
            return Ok(());
        }
    };

    let action = {
        let mut seen = state.seen_event_ids.lock().await;
        process_frame(&frame, &mut seen)
    };

    match action {
        FrameAction::Pong => {
            tracing::debug!("WS ping (heartbeat)");
            // Answer the server's keepalive ping so it doesn't consider the
            // connection dead. The Lark SDK sends a pong for every ping.
            let pong_bytes = pbbp2::pong(&frame);
            if let Err(e) = ws
                .send(tokio_tungstenite::tungstenite::Message::Binary(pong_bytes.into()))
                .await
            {
                tracing::warn!("WS pong send failed: {}", e);
            }
        }
        FrameAction::Ack => send_event_ack(ws, &frame).await,
        FrameAction::Message(event) => {
            // Ack EVERY message event first: at-least-once, an unacked event is
            // re-delivered forever and a client that never acks is eventually
            // treated as dead.
            send_event_ack(ws, &frame).await;
            let Some(event_data) = event.event else {
                return Ok(());
            };
            let Some(msg_data) = event_data.message else {
                return Ok(());
            };
            let payload = &frame.payload;
            let payload_str = String::from_utf8_lossy(payload);
            tracing::info!(
                "WS event payload: {}",
                &payload_str.chars().take(300).collect::<String>()
            );
            tracing::info!("WS event: type=im.message.receive_v1");

            let mut text = parse_message_content(&msg_data);
            // Replace @mention placeholders (`@_user_N`) with real names so the
            // AI sees who was referenced; the bot's own mention is dropped.
            // Feishu otherwise leaks opaque `@_user_1` tokens into the prompt.
            if !msg_data.mentions.is_empty() {
                let bot_id = state.bot_open_id(feishu).await.unwrap_or_default();
                if let Some(eid) = event.header.as_ref().and_then(|h| h.event_id.clone())
                    && is_mentioned(&msg_data.mentions, &bot_id)
                {
                    tracing::info!("event {} mentions bot", eid);
                }
                text = strip_mentions(&text, &msg_data.mentions, &bot_id);
            }
            let thread_id = msg_data.thread_id.clone();
            // Who sent the message: needed for the group completion notice.
            let sender_open_id = event_data
                .sender
                .as_ref()
                .and_then(|s| s.sender_id.as_ref())
                .and_then(|i| i.open_id.clone());
            tracing::info!(
                "Message: chat={} type={} thread={} text={}",
                msg_data.chat_id,
                msg_data.chat_type,
                thread_id.as_deref().unwrap_or("-"),
                &text.chars().take(50).collect::<String>()
            );
            // Images embedded in this message (image/post) are downloaded on the
            // spawned task so the WS read loop keeps reading (heartbeats, new
            // messages, card actions) while a download is in flight.
            let image_keys = extract_image_keys(&msg_data.content, &msg_data.message_type);
            let msg_id = msg_data.message_id.clone();
            let chat_id = msg_data.chat_id.clone();
            let chat_type = msg_data.chat_type.clone();
            let parent_id = msg_data.parent_id.clone();
            let sink = sink.clone();
            let feishu = feishu.clone();
            tokio::spawn(async move {
                let mut images = Vec::new();
                for key in &image_keys {
                    match feishu.download_image(&msg_id, key).await {
                        Ok(img) => images.push(img),
                        Err(e) => tracing::warn!("download image {} failed: {}", key, e),
                    }
                }
                sink.handle_message(crate::bridge::IncomingMessage {
                    message_id: msg_id,
                    chat_id,
                    chat_type,
                    thread_id,
                    parent_id,
                    text,
                    images,
                    requester_open_id: sender_open_id,
                })
                .await;
            });
        }
        FrameAction::CardAction(value) => {
            // Ack ALWAYS — even an unparseable card action must be acked,
            // otherwise Feishu re-delivers it forever (pitfall 8).
            tracing::debug!("card action value: {:?}", value.to_string());
            let result = sink.handle_card_action(value).await;
            let resp_bytes = build_response_frame(&frame, result.as_ref());
            if let Err(e) = ws
                .send(tokio_tungstenite::tungstenite::Message::Binary(resp_bytes.into()))
                .await
            {
                tracing::warn!("WS response send failed: {}", e);
            } else {
                tracing::info!("Sent card action ack");
            }
        }
        FrameAction::None => {
            tracing::debug!("WS frame ignored (type={:?})", frame.headers.get("type"));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full v1 event payload (as extracted from a frame) must deserialize
    /// into MessageReceiveEvent and reach the message fields the handler uses.
    #[test]
    fn frame_payload_deserializes_to_receive_v1() {
        let payload = br#"{
            "schema": "2.0",
            "header": {
                "event_id": "e1",
                "event_type": "im.message.receive_v1",
                "create_time": "1609295409000",
                "token": "t",
                "app_id": "cli_1",
                "tenant_key": "tk"
            },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_1" } },
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_1",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"hi\"}"
                }
            }
        }"#;

        let event: MessageReceiveEvent = serde_json::from_slice(payload).unwrap();
        let event_data = event.event.expect("event present");
        let msg = event_data.message.expect("message present");

        // This mirrors handle_binary_frame's routing decision.
        assert_eq!(msg.thread_id, None);
        assert_eq!(parse_message_content(&msg), "hi");

        // And the dedup key used by the handler:
        let event_id = event.header.as_ref().and_then(|h| h.event_id.clone());
        assert_eq!(event_id.as_deref(), Some("e1"));
    }

    /// build_response_frame must echo the request's routing fields so Feishu
    /// can match the ack to the original card action.
    #[test]
    fn response_frame_echoes_request_fields() {
        let bytes = pbbp2::encode(
            &Routing {
                seq_id: 99,
                log_id: 5,
                service: 12,
                method: 2,
                payload_encoding: Some("json".to_string()),
                payload_type: Some("event".to_string()),
                log_id_new: Some("log-new-9".to_string()),
            },
            &[("type", "card")],
            b"{}",
        );

        let request = Frame::decode(&bytes).unwrap();
        let resp = build_response_frame(&request, None);

        let parsed = Frame::decode(&resp).expect("response parses");
        assert_eq!(parsed.routing.seq_id, 99);
        assert_eq!(parsed.routing.log_id, 5);
        assert_eq!(parsed.routing.service, 12);
        assert_eq!(parsed.routing.method, 2);
        assert_eq!(parsed.routing.log_id_new.as_deref(), Some("log-new-9"));
        // payload is a JSON string {"code":200,"headers":null,"data":"<b64>"}
        let payload_str = String::from_utf8_lossy(&parsed.payload);
        let v: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
        assert_eq!(v["code"], 200);
        assert!(v["data"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
        // headers echoed back (plus biz_rt)
        assert!(parsed.headers.len() >= 2);
    }

    /// A card callback ack carries an optional Toast (instant client feedback)
    /// plus the JSON 2.0 result card that replaces the interactive buttons.
    #[test]
    fn card_response_includes_toast_and_2_0_card() {
        use base64::Engine;

        let bytes = pbbp2::encode(
            &Routing {
                seq_id: 1,
                service: 12,
                method: 2,
                ..Routing::default()
            },
            &[("type", "card")],
            b"{}",
        );

        let request = Frame::decode(&bytes).unwrap();
        let result = crate::bridge::handler::CardActionResult {
            card: Some(serde_json::json!({
                "schema": "2.0",
                "header": { "title": { "tag": "plain_text", "content": "✅ 已允许一次" }, "template": "green" },
                "body": { "elements": [] }
            })),
            toast: Some("已允许本次执行".to_string()),
        };
        let resp = build_response_frame(&request, Some(&result));

        let parsed = Frame::decode(&resp).expect("response parses");
        let payload_str = String::from_utf8_lossy(&parsed.payload);
        let v: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
        let data_b64 = v["data"].as_str().unwrap();
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .unwrap(),
        )
        .unwrap();
        let inner: serde_json::Value = serde_json::from_str(&decoded).unwrap();
        assert_eq!(inner["toast"]["type"], "success");
        assert_eq!(inner["toast"]["content"], "已允许本次执行");
        assert_eq!(inner["card"]["type"], "raw");
        assert_eq!(inner["card"]["data"]["schema"], "2.0");
        assert_eq!(
            inner["card"]["data"]["header"]["title"]["content"],
            "✅ 已允许一次"
        );
    }

    /// Every WS event (message receive, bot added, ...) must be acked with a
    /// code-200 frame, exactly like the Lark SDK does for each event. Feishu
    /// re-delivers events it never sees an ack for, then stops pushing.
    #[test]
    fn event_ack_frame_has_code_200_and_null_data() {
        let bytes = pbbp2::encode(
            &Routing {
                seq_id: 7,
                log_id: 3,
                service: 12,
                method: 1,
                ..Routing::default()
            },
            &[("type", "event")],
            br#"{"header":{"event_type":"im.message.receive_v1"}}"#,
        );

        let request = Frame::decode(&bytes).unwrap();
        let ack = build_event_ack_frame(&request);

        let parsed = Frame::decode(&ack).expect("ack parses");
        assert_eq!(parsed.routing.seq_id, 7);
        assert_eq!(parsed.routing.service, 12);
        // payload must be {"code":200,"headers":null,"data":null}
        let payload_str = String::from_utf8_lossy(&parsed.payload);
        let v: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
        assert_eq!(v["code"], 200);
        assert_eq!(v["data"], serde_json::Value::Null);
    }

    /// A folding button group (overflow) click delivers `action.option` as the
    /// encoded "qi|label" string; it must decode back into the standard answer.
    #[test]
    fn overflow_option_decodes_into_question_answer() {
        let payload = br#"{
            "schema": "2.0",
            "event": {
                "action": {
                    "tag": "overflow",
                    "value": {
                        "action": "question",
                        "reply": "answer",
                        "request_id": "que_1",
                        "session_id": "ses_1"
                    },
                    "option": "2|/a2"
                }
            }
        }"#;
        let value = extract_card_action_value(payload).expect("value extracted");
        assert_eq!(value["question_index"], 2);
        assert_eq!(value["answer"], "/a2");
        assert_eq!(value["request_id"], "que_1");
    }

    /// A form-submitted custom answer arrives as `action.form_value`; the typed
    /// text must become `answer` on the button's routing payload.
    #[test]
    fn form_custom_answer_becomes_answer() {
        let payload = r#"{
            "schema": "2.0",
            "event": {
                "action": {
                    "tag": "button",
                    "name": "submit_0",
                    "value": {
                        "action": "question",
                        "reply": "answer",
                        "request_id": "que_1",
                        "session_id": "ses_1",
                        "question_index": 0
                    },
                    "form_value": {
                        "custom_0": "自己写的答案"
                    }
                }
            }
        }"#;
        let value = extract_card_action_value(payload.as_bytes()).expect("value extracted");
        assert_eq!(value["answer"], "自己写的答案");
        assert_eq!(value["question_index"], 0);
        assert_eq!(value["request_id"], "que_1");
    }

    /// Form submit callbacks may omit `action.value` entirely — the routing
    /// payload is then rebuilt from the button `name`. The directory is no
    /// longer part of the name (Feishu caps `name` at 100 chars), so the rebuilt
    /// value carries no `directory`; the handler re-resolves it.
    #[test]
    fn form_submit_without_value_rebuilds_routing_from_name() {
        let payload = r#"{
            "schema": "2.0",
            "event": {
                "action": {
                    "tag": "button",
                    "name": "submit|que_1|ses_1|0",
                    "form_value": {
                        "custom_0": "自定义答案"
                    }
                }
            }
        }"#;
        let value = extract_card_action_value(payload.as_bytes()).expect("value extracted");
        assert_eq!(value["answer"], "自定义答案");
        assert_eq!(value["question_index"], 0);
        assert_eq!(value["request_id"], "que_1");
        assert_eq!(value["session_id"], "ses_1");
        assert!(value.get("directory").is_none(), "directory dropped from name");
        assert_eq!(value["reply"], "answer");
    }

    /// Legacy cards still carry the directory as a 5th name segment; keep
    /// parsing them so in-flight cards from before the change keep working.
    #[test]
    fn form_submit_without_value_accepts_legacy_directory_segment() {
        let payload = r#"{
            "schema": "2.0",
            "event": {
                "action": {
                    "tag": "button",
                    "name": "submit|que_1|ses_1|0|/tmp/proj",
                    "form_value": {
                        "custom_0": "自定义答案"
                    }
                }
            }
        }"#;
        let value = extract_card_action_value(payload.as_bytes()).expect("value extracted");
        assert_eq!(value["answer"], "自定义答案");
        assert_eq!(value["directory"], "/tmp/proj");
        assert_eq!(value["session_id"], "ses_1");
    }

    /// A plain button click keeps its value untouched (no option/form_value).
    #[test]
    fn plain_button_value_unchanged() {
        let payload = br#"{
            "schema": "2.0",
            "event": {
                "action": {
                    "tag": "button",
                    "value": {
                        "action": "question",
                        "reply": "answer",
                        "request_id": "que_1",
                        "session_id": "ses_1",
                        "question_index": 1,
                        "answer": "/b"
                    }
                }
            }
        }"#;
        let value = extract_card_action_value(payload).expect("value extracted");
        assert_eq!(value["answer"], "/b");
        assert_eq!(value["question_index"], 1);
    }
}

// --- process_frame: pure dedupe / age / dispatch decisions ---

/// Build a frame whose payload is the given JSON, typed "event".
#[cfg(test)]
fn event_frame(payload: &[u8]) -> Frame {
    let bytes = pbbp2::encode(
        &Routing {
            seq_id: 1,
            service: 1,
            method: 1,
            ..Routing::default()
        },
        &[("type", "event")],
        payload,
    );
    Frame::decode(&bytes).expect("frame parses")
}

/// Build a header-only frame of the given `type` (control / unknown frames).
#[cfg(test)]
fn header_frame(kind: &str) -> Frame {
    let bytes = pbbp2::encode(&Routing::default(), &[("type", kind)], b"");
    Frame::decode(&bytes).expect("frame parses")
}

#[cfg(test)]
fn receive_payload(event_id: &str, create_time_ms: i64) -> Vec<u8> {
    format!(
            r#"{{"header":{{"event_id":"{}","event_type":"im.message.receive_v1","create_time":"{}"}},"event":{{"sender":{{"sender_id":{{"open_id":"ou_1"}}}},"message":{{"message_id":"om_1","chat_id":"oc_1","chat_type":"p2p","message_type":"text","content":"{{\"text\":\"hi\"}}"}}}}}}"#,
            event_id, create_time_ms
        )
        .into_bytes()
}

#[test]
fn process_frame_routes_ping_and_unknown() {
    // ping → Pong.
    let frame = header_frame("ping");
    assert!(matches!(
        process_frame(&frame, &mut DedupeSet::new(10)),
        FrameAction::Pong
    ));

    // Unknown type → None (no ack, no dispatch).
    let frame = header_frame("card");
    assert!(matches!(
        process_frame(&frame, &mut DedupeSet::new(10)),
        FrameAction::None
    ));
}

#[test]
fn process_frame_dispatches_fresh_message() {
    let frame = event_frame(&receive_payload("e_fresh", chrono::Utc::now().timestamp_millis()));
    let mut seen = DedupeSet::new(10);
    match process_frame(&frame, &mut seen) {
        FrameAction::Message(ev) => {
            let msg = ev.event.unwrap().message.unwrap();
            assert_eq!(parse_message_content(&msg), "hi");
        }
        other => panic!("expected Message, got {:?}", std::mem::discriminant(&other)),
    }
}

#[test]
fn process_frame_dedupes_a_replayed_event_but_acks_it() {
    let frame = event_frame(&receive_payload("e_dup", chrono::Utc::now().timestamp_millis()));
    let mut seen = DedupeSet::new(10);
    // First delivery: dispatched.
    assert!(matches!(
        process_frame(&frame, &mut seen),
        FrameAction::Message(_)
    ));
    // Re-delivery: acked (so Feishu stops) but NOT dispatched again.
    assert!(matches!(process_frame(&frame, &mut seen), FrameAction::Ack));
}

#[test]
fn process_frame_acks_stale_events_without_dispatch() {
    let old = chrono::Utc::now().timestamp_millis() - 400_000;
    let frame = event_frame(&receive_payload("e_old", old));
    let mut seen = DedupeSet::new(10);
    assert!(matches!(process_frame(&frame, &mut seen), FrameAction::Ack));
}

#[test]
fn process_frame_acks_unparseable_and_unknown_events() {
    // Unparseable payload → ack (stop re-delivery), no dispatch.
    let frame = event_frame(b"not json");
    assert!(matches!(
        process_frame(&frame, &mut DedupeSet::new(10)),
        FrameAction::Ack
    ));
    // Parseable but a non-message event type → ack, no dispatch.
    let payload = br#"{"header":{"event_type":"bot.added_v1"}}"#;
    let frame = event_frame(payload);
    assert!(matches!(
        process_frame(&frame, &mut DedupeSet::new(10)),
        FrameAction::Ack
    ));
}

#[test]
fn process_frame_routes_card_actions() {
    let payload = br#"{
            "header": { "event_type": "card.action.trigger", "event_id": "e_card" },
            "event": {
                "action": {
                    "tag": "button",
                    "value": { "action": "perm", "reply": "once", "request_id": "p1", "session_id": "s1" }
                }
            }
        }"#;
    let frame = event_frame(payload);
    match process_frame(&frame, &mut DedupeSet::new(10)) {
        FrameAction::CardAction(v) => assert_eq!(v["request_id"], "p1"),
        other => panic!("expected CardAction, got {:?}", std::mem::discriminant(&other)),
    }
}

#[test]
fn card_action_carries_open_message_id_from_context() {
    // `/topic --adopt`'s card button (ADR-0016) anchors the new topic on the
    // card's own message id; the schema 2.0 callback carries it on
    // `event.context.open_message_id`.
    let payload = br#"{
            "header": { "event_type": "card.action.trigger", "event_id": "e_card" },
            "event": {
                "action": {
                    "tag": "button",
                    "value": { "action": "switch", "op": "topic_adopt", "chat_id": "oc_1", "thread_id": "oc_1", "session_id": "ses_abc" }
                },
                "context": { "open_message_id": "om_switch_card", "open_chat_id": "oc_1" }
            }
        }"#;
    let frame = event_frame(payload);
    match process_frame(&frame, &mut DedupeSet::new(10)) {
        FrameAction::CardAction(v) => {
            assert_eq!(v["op"], "topic_adopt");
            assert_eq!(v["open_message_id"], "om_switch_card");
        }
        other => panic!("expected CardAction, got {:?}", std::mem::discriminant(&other)),
    }
}

#[test]
fn card_action_falls_back_to_open_message_id_on_action() {
    // Older callback shapes put the message id on the action object; the
    // extraction still threads it through.
    let payload = br#"{
            "header": { "event_type": "card.action.trigger", "event_id": "e_card2" },
            "event": {
                "action": {
                    "tag": "button",
                    "open_message_id": "om_switch_card",
                    "value": { "action": "switch", "op": "topic_adopt", "chat_id": "oc_1", "thread_id": "oc_1", "session_id": "ses_abc" }
                }
            }
        }"#;
    let frame = event_frame(payload);
    match process_frame(&frame, &mut DedupeSet::new(10)) {
        FrameAction::CardAction(v) => assert_eq!(v["open_message_id"], "om_switch_card"),
        other => panic!("expected CardAction, got {:?}", std::mem::discriminant(&other)),
    }
}

// --- DedupeSet: bounded, evicts on overflow ---

#[test]
fn dedupe_set_evicts_when_over_cap() {
    let mut seen = DedupeSet::new(3);
    assert!(!seen.check_and_insert("a"));
    assert!(!seen.check_and_insert("b"));
    assert!(!seen.check_and_insert("c"));
    assert_eq!(seen.len(), 3);
    // Inserting a 4th clears the window first.
    assert!(!seen.check_and_insert("d"));
    assert_eq!(seen.len(), 1);
    // The evicted ids are no longer "seen".
    assert!(!seen.check_and_insert("a"));
}
