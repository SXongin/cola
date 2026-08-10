use crate::bridge::handler::App;
use crate::feishu::event::{MessageData, MessageReceiveEvent};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::time::{Duration, sleep};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const MAX_RECONNECT_DELAY_SECS: u64 = 30;

// --- Minimal protobuf varint parser for Feishu's pbbp2 frame ---

fn read_varint(data: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut pos = start;
    while pos < data.len() {
        let byte = data[pos];
        pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, pos));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

/// A parsed Feishu WS frame (protobuf Frame message).
struct ParsedFrame {
    seq_id: u64,
    log_id: u64,
    service: i32,
    method: i32,
    headers: HashMap<String, String>,
    payload_encoding: Option<String>,
    payload_type: Option<String>,
    payload: Vec<u8>,
    log_id_new: Option<String>,
}

/// Parse a Feishu WS binary frame (protobuf Frame message).
fn parse_frame(data: &[u8]) -> Option<ParsedFrame> {
    let mut pos = 0;
    let mut frame = ParsedFrame {
        seq_id: 0,
        log_id: 0,
        service: 0,
        method: 0,
        headers: HashMap::new(),
        payload_encoding: None,
        payload_type: None,
        payload: Vec::new(),
        log_id_new: None,
    };

    while pos < data.len() {
        let (tag, p) = read_varint(data, pos)?;
        pos = p;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u32;

        match (field_num, wire_type) {
            (1, 0) => {
                // SeqID (uint64)
                let (v, p) = read_varint(data, pos)?;
                pos = p;
                frame.seq_id = v;
            }
            (2, 0) => {
                // LogID (uint64)
                let (v, p) = read_varint(data, pos)?;
                pos = p;
                frame.log_id = v;
            }
            (3, 0) => {
                // Service (int32)
                let (v, p) = read_varint(data, pos)?;
                pos = p;
                frame.service = v as i32;
            }
            (4, 0) => {
                // Method (int32)
                let (v, p) = read_varint(data, pos)?;
                pos = p;
                frame.method = v as i32;
            }
            (5, 2) => {
                // Header entry (nested message with key=1, value=2)
                let (len, p) = read_varint(data, pos)?;
                pos = p;
                if pos + len as usize > data.len() {
                    return None;
                }
                let hdr = &data[pos..pos + len as usize];
                pos += len as usize;
                if let Some((k, v)) = parse_header(hdr) {
                    frame.headers.insert(k, v);
                }
            }
            (6, 2) => {
                // PayloadEncoding (string)
                let (len, p) = read_varint(data, pos)?;
                pos = p;
                if pos + len as usize > data.len() {
                    return None;
                }
                frame.payload_encoding =
                    Some(String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string());
                pos += len as usize;
            }
            (7, 2) => {
                // PayloadType (string)
                let (len, p) = read_varint(data, pos)?;
                pos = p;
                if pos + len as usize > data.len() {
                    return None;
                }
                frame.payload_type =
                    Some(String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string());
                pos += len as usize;
            }
            (8, 2) => {
                // Payload (raw bytes, typically JSON)
                let (len, p) = read_varint(data, pos)?;
                pos = p;
                if pos + len as usize > data.len() {
                    return None;
                }
                frame.payload = data[pos..pos + len as usize].to_vec();
                pos += len as usize;
            }
            (9, 2) => {
                // LogIDNew (string)
                let (len, p) = read_varint(data, pos)?;
                pos = p;
                if pos + len as usize > data.len() {
                    return None;
                }
                frame.log_id_new =
                    Some(String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string());
                pos += len as usize;
            }
            // Skip other fields
            (_, 0) => {
                let (_, p) = read_varint(data, pos)?;
                pos = p;
            }
            (_, 2) => {
                let (len, p) = read_varint(data, pos)?;
                pos = p + len as usize;
            }
            (_, 5) => pos += 4,
            (_, 1) => pos += 8,
            _ => return None,
        }
    }

    Some(frame)
}

fn parse_header(data: &[u8]) -> Option<(String, String)> {
    let mut pos = 0;
    let mut key = String::new();
    let mut value = String::new();

    while pos < data.len() {
        let (tag, p) = read_varint(data, pos)?;
        pos = p;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u32;

        if wire_type == 2 {
            let (len, p) = read_varint(data, pos)?;
            pos = p;
            if pos + len as usize > data.len() {
                return None;
            }
            let s = String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string();
            pos += len as usize;
            match field_num {
                1 => key = s,
                2 => value = s,
                _ => {}
            }
        } else if wire_type == 0 {
            let (_, p) = read_varint(data, pos)?;
            pos = p;
        }
    }

    Some((key, value))
}

/// Build a response frame for a received card action. Echoes the request
/// frame's fields (seq_id, log_id, service, method, headers, payload_*)
/// and sets the payload to the ack JSON, matching the Lark SDK's Response
/// format: {"code":200,"headers":null,"data":"<base64>"}.
fn build_response_frame(request: &ParsedFrame, result_card: Option<&serde_json::Value>) -> Option<Vec<u8>> {
    use base64::Engine;

    // CardActionTriggerResponse: optionally update the card with the result.
    // Format: {"card":{"type":"raw","data":{card json}}}
    let rsp_json = if let Some(card) = result_card {
        serde_json::json!({"card": {"type": "raw", "data": card}}).to_string()
    } else {
        "{}".to_string()
    };
    // Response: {"code":200,"headers":null,"data":"<base64 of rsp>"}
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(rsp_json.as_bytes());
    let payload = format!(r#"{{"code":200,"headers":null,"data":"{}"}}"#, data_b64);

    Some(encode_response_frame(request, &payload))
}

/// Build the ack frame Feishu requires for EVERY ordinary event (message
/// receive, bot added, ...). The Lark SDK sends this after handling each
/// `MessageTypeEvent`; without it Feishu re-delivers the event forever and
/// eventually stops pushing new ones. Same routing echo as the card response,
/// but the payload data is null (no card to update).
fn build_event_ack_frame(request: &ParsedFrame) -> Option<Vec<u8>> {
    Some(encode_response_frame(
        request,
        r#"{"code":200,"headers":null,"data":null}"#,
    ))
}

/// Answer a pbbp2 control "ping" frame with a "pong" so the server keeps the
/// connection alive. Echoes routing fields; the headers type becomes "pong".
fn build_pong_frame(request: &ParsedFrame) -> Option<Vec<u8>> {
    let mut out = Vec::new();

    // Field 1: seq_id (varint)
    if request.seq_id != 0 {
        encode_varint_field(&mut out, 1, request.seq_id);
    }
    // Field 2: log_id (varint)
    if request.log_id != 0 {
        encode_varint_field(&mut out, 2, request.log_id);
    }
    // Field 3: service (varint)
    encode_varint_field(&mut out, 3, request.service as u64);
    // Field 4: method (varint)
    encode_varint_field(&mut out, 4, request.method as u64);

    // Field 5: headers — reply type=pong, plus biz_rt
    {
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "type");
        encode_string_field(&mut hdr, 2, "pong");
        encode_bytes_field(&mut out, 5, &hdr);
    }
    {
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "biz_rt");
        encode_string_field(&mut hdr, 2, "1");
        encode_bytes_field(&mut out, 5, &hdr);
    }

    // Field 6: payload_encoding
    if let Some(pe) = &request.payload_encoding {
        encode_string_field(&mut out, 6, pe);
    }
    // Field 7: payload_type
    if let Some(pt) = &request.payload_type {
        encode_string_field(&mut out, 7, pt);
    }
    // Field 8: payload (empty)
    encode_bytes_field(&mut out, 8, b"");
    // Field 9: log_id_new
    if let Some(lin) = &request.log_id_new {
        encode_string_field(&mut out, 9, lin);
    }

    Some(out)
}

/// Build a keepalive ping frame cola sends proactively (mirroring the Lark
/// SDK's pingLoop) so an idle connection is detected as dead and reconnects.
fn build_ping_frame() -> Vec<u8> {
    let mut out = Vec::new();

    // Field 3: service (varint) — 0 is fine for a control frame
    encode_varint_field(&mut out, 3, 0);
    // Field 4: method (varint) — FrameTypeControl
    encode_varint_field(&mut out, 4, 0);

    // Field 5: headers — type=ping
    {
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "type");
        encode_string_field(&mut hdr, 2, "ping");
        encode_bytes_field(&mut out, 5, &hdr);
    }

    // Field 8: payload (empty)
    encode_bytes_field(&mut out, 8, b"");

    out
}

/// Shared frame writer: echoes the request's routing fields and headers and
/// places `payload` in field 8.
fn encode_response_frame(request: &ParsedFrame, payload: &str) -> Vec<u8> {
    let mut out = Vec::new();

    // Field 1: seq_id (varint)
    if request.seq_id != 0 {
        encode_varint_field(&mut out, 1, request.seq_id);
    }
    // Field 2: log_id (varint)
    if request.log_id != 0 {
        encode_varint_field(&mut out, 2, request.log_id);
    }
    // Field 3: service (varint)
    encode_varint_field(&mut out, 3, request.service as u64);
    // Field 4: method (varint)
    encode_varint_field(&mut out, 4, request.method as u64);

    // Field 5: headers (length-delimited, repeated) — echo ALL request headers
    for (k, v) in &request.headers {
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, k);
        encode_string_field(&mut hdr, 2, v);
        encode_bytes_field(&mut out, 5, &hdr);
    }
    // Add biz_rt header (processing time ms)
    {
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "biz_rt");
        encode_string_field(&mut hdr, 2, "1");
        encode_bytes_field(&mut out, 5, &hdr);
    }

    // Field 6: payload_encoding
    if let Some(pe) = &request.payload_encoding {
        encode_string_field(&mut out, 6, pe);
    }
    // Field 7: payload_type
    if let Some(pt) = &request.payload_type {
        encode_string_field(&mut out, 7, pt);
    }
    // Field 8: payload (bytes)
    encode_bytes_field(&mut out, 8, payload.as_bytes());
    // Field 9: log_id_new
    if let Some(lin) = &request.log_id_new {
        encode_string_field(&mut out, 9, lin);
    }

    out
}

fn encode_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    let tag = (field << 3) | 0; // wire type 0 = varint
    encode_varint(out, tag as u64);
    encode_varint(out, value);
}

fn encode_string_field(out: &mut Vec<u8>, field: u32, value: &str) {
    encode_bytes_field(out, field, value.as_bytes());
}

fn encode_bytes_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    let tag = (field << 3) | 2; // wire type 2 = length-delimited
    encode_varint(out, tag as u64);
    encode_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

fn encode_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

// --- WebSocket event loop ---

pub async fn event_loop(app: &Arc<App>) -> crate::error::Result<()> {
    let mut attempt = 0;
    loop {
        match connect_and_listen(app).await {
            Ok(()) => tracing::info!("WebSocket closed cleanly, reconnecting..."),
            Err(e) => tracing::warn!("WebSocket error: {}, reconnecting...", e),
        }
        let delay = (2u64.pow(attempt.min(5))).min(MAX_RECONNECT_DELAY_SECS);
        tracing::info!("Reconnecting in {}s (attempt {})", delay, attempt + 1);
        sleep(Duration::from_secs(delay)).await;
        attempt += 1;
    }
}

async fn connect_and_listen(app: &Arc<App>) -> crate::error::Result<()> {
    let ws_url = app.feishu.get_ws_endpoint().await?;
    tracing::info!("WS endpoint resolved");
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .map_err(|e| crate::error::BridgeError::Feishu(format!("WS connect failed: {}", e)))?;
    tracing::info!("Connected to Feishu WebSocket");
    handle_connection(ws_stream, app).await
}

async fn handle_connection(
    mut ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    app: &Arc<App>,
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
                    .send(tokio_tungstenite::tungstenite::Message::Binary(build_ping_frame().into()))
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
                            handle_binary_frame(&data, &mut ws, app).await?;
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
async fn send_event_ack(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, frame: &ParsedFrame) {
    if let Some(ack_bytes) = build_event_ack_frame(frame)
        && let Err(e) = ws
            .send(tokio_tungstenite::tungstenite::Message::Binary(ack_bytes.into()))
            .await
    {
        tracing::warn!("WS event ack send failed: {}", e);
    }
}

async fn handle_binary_frame(
    data: &[u8],
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    app: &Arc<App>,
) -> crate::error::Result<()> {
    let frame = match parse_frame(data) {
        Some(v) => v,
        None => {
            tracing::warn!("Failed to parse WS binary frame (len={})", data.len());
            return Ok(());
        }
    };

    let msg_type = frame.headers.get("type").map(|s| s.as_str()).unwrap_or("unknown");

    match msg_type {
        "ping" => {
            tracing::debug!("WS ping (heartbeat)");
            // Answer the server's keepalive ping so it doesn't consider the
            // connection dead. The Lark SDK sends a pong for every ping.
            if let Some(pong_bytes) = build_pong_frame(&frame) {
                if let Err(e) = ws
                    .send(tokio_tungstenite::tungstenite::Message::Binary(pong_bytes.into()))
                    .await
                {
                    tracing::warn!("WS pong send failed: {}", e);
                }
            }
        }
        "event" => {
            let payload = &frame.payload;
            let payload_str = String::from_utf8_lossy(payload);
            tracing::info!(
                "WS event payload: {}",
                &payload_str.chars().take(300).collect::<String>()
            );

            let event_type = serde_json::from_slice::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| {
                    v.get("header")?
                        .get("event_type")?
                        .as_str()
                        .map(|s| s.to_string())
                })
                .unwrap_or_default();

            tracing::info!("WS event: type={}", event_type);

            if event_type == "im.message.receive_v1" {
                // Ack EVERY event first. Feishu's long-connection protocol is
                // at-least-once: an unacked event is re-delivered (and re-delivered),
                // and a client that never acks is eventually treated as dead and
                // stops receiving new events entirely. Without this the bot appears
                // unresponsive to all new messages.
                send_event_ack(ws, &frame).await;

                // Dedup by event_id from JSON payload (stable across retries)
                let event_id = serde_json::from_slice::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|v| v.get("header")?.get("event_id")?.as_str().map(|s| s.to_string()));

                if let Some(ref eid) = event_id {
                    let mut seen = app.seen_event_ids.lock().await;
                    if seen.contains(eid) {
                        tracing::info!("Deduped event_id={}", &eid[..eid.len().min(30)]);
                        return Ok(());
                    }
                    seen.insert(eid.clone());
                }

                // Filter: skip events older than 5 minutes (Feishu replays old unacked events)
                let now_ms = chrono::Utc::now().timestamp_millis();
                let event_age = serde_json::from_slice::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|v| v.get("header")?.get("create_time")?.as_str()?.parse::<i64>().ok())
                    .map(|ct| now_ms - ct);

                if let Some(age_ms) = event_age
                    && age_ms > 300_000
                {
                    tracing::info!("Skipping old event (age={}s)", age_ms / 1000);
                    return Ok(());
                }

                if let Ok(event) = serde_json::from_slice::<MessageReceiveEvent>(&payload) {
                    if let Some(event_data) = event.event
                        && let Some(msg_data) = event_data.message
                    {
                        let text = parse_message_content(&msg_data);
                        let thread_id = msg_data.thread_id.clone();
                        tracing::info!(
                            "Message: chat={} type={} thread={} text={}",
                            msg_data.chat_id,
                            msg_data.chat_type,
                            thread_id.as_deref().unwrap_or("-"),
                            &text.chars().take(50).collect::<String>()
                        );
                        // Handle the message on a separate task so the WS read loop
                        // keeps reading (heartbeats, new messages, card actions) while
                        // a prompt is in flight. Blocking here wedges the entire
                        // connection and Feishu eventually drops it (CLOSE-WAIT).
                        let app = app.clone();
                        tokio::spawn(async move {
                            app.handle_message(
                                msg_data.message_id,
                                msg_data.chat_id,
                                msg_data.chat_type,
                                thread_id,
                                text,
                            )
                            .await;
                        });
                    }
                }
            } else if event_type == "card.action.trigger" {
                // card.action.trigger has event.action.value (not event.value).
                let action_value = serde_json::from_slice::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|v| {
                        v.get("event")
                            .and_then(|e| e.get("action"))
                            .and_then(|a| a.get("value"))
                            .cloned()
                    });
                // Ack ALWAYS — even an unparseable card action must be acked,
                // otherwise Feishu re-delivers it forever (pitfall 8).
                let result_card = match action_value {
                    Some(value) => app.handle_card_action(value).await,
                    None => None,
                };
                let resp = build_response_frame(&frame, result_card.as_ref());
                if let Some(resp_bytes) = resp {
                    if let Err(e) = ws
                        .send(tokio_tungstenite::tungstenite::Message::Binary(resp_bytes.into()))
                        .await
                    {
                        tracing::warn!("WS response send failed: {}", e);
                    } else {
                        tracing::info!("Sent card action ack");
                    }
                }
            } else {
                // Any other event type (bot added, message recalled, ...) still
                // needs an ack, otherwise Feishu re-delivers it forever.
                send_event_ack(ws, &frame).await;
            }
        }
        "card" => {
            tracing::debug!("WS card action");
        }
        other => {
            tracing::debug!("WS unknown type: {} headers={:?}", other, frame.headers);
        }
    }

    Ok(())
}

fn parse_message_content(msg: &MessageData) -> String {
    if msg.message_type == "text"
        && let Ok(content) = serde_json::from_str::<serde_json::Value>(&msg.content)
    {
        return content["text"].as_str().unwrap_or("").to_string();
    }
    msg.content.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_message() {
        let msg = MessageData {
            message_id: "msg_1".into(),
            root_id: None,
            parent_id: None,
            thread_id: None,
            chat_id: "chat_1".into(),
            chat_type: "p2p".into(),
            message_type: "text".into(),
            content: r#"{"text": "hello world"}"#.into(),
        };
        assert_eq!(parse_message_content(&msg), "hello world");
    }

    #[test]
    fn parse_non_text_message() {
        let msg = MessageData {
            message_id: "msg_2".into(),
            root_id: Some("root_1".into()),
            parent_id: None,
            thread_id: None,
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            message_type: "image".into(),
            content: r#"{"image_key": "abc123"}"#.into(),
        };
        assert_eq!(parse_message_content(&msg), r#"{"image_key": "abc123"}"#);
    }

    /// Build a WS frame the same way the encode helpers write it, then confirm
    /// parse_frame recovers every field.
    #[test]
    fn frame_round_trip() {
        let mut bytes = Vec::new();
        encode_varint_field(&mut bytes, 1, 42); // seq_id
        encode_varint_field(&mut bytes, 2, 7); // log_id
        encode_varint_field(&mut bytes, 3, 12); // service
        encode_varint_field(&mut bytes, 4, 3); // method

        // header: type=event
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "type");
        encode_string_field(&mut hdr, 2, "event");
        encode_bytes_field(&mut bytes, 5, &hdr);

        encode_string_field(&mut bytes, 6, "json"); // payload_encoding
        encode_string_field(&mut bytes, 7, "event"); // payload_type
        encode_bytes_field(&mut bytes, 8, br#"{"header":{"event_type":"im.message.receive_v1"}}"#);
        encode_string_field(&mut bytes, 9, "log-new-1"); // log_id_new

        let frame = parse_frame(&bytes).expect("frame parses");
        assert_eq!(frame.seq_id, 42);
        assert_eq!(frame.log_id, 7);
        assert_eq!(frame.service, 12);
        assert_eq!(frame.method, 3);
        assert_eq!(frame.headers.get("type").map(|s| s.as_str()), Some("event"));
        assert_eq!(frame.payload_encoding.as_deref(), Some("json"));
        assert_eq!(frame.payload_type.as_deref(), Some("event"));
        assert_eq!(frame.log_id_new.as_deref(), Some("log-new-1"));
        let payload: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(
            payload["header"]["event_type"],
            serde_json::json!("im.message.receive_v1")
        );
    }

    /// The dispatch logic in handle_binary_frame derives the event type from
    /// header.event_type in the frame payload — verify that extraction.
    #[test]
    fn event_type_extraction_from_frame() {
        let mut bytes = Vec::new();
        encode_varint_field(&mut bytes, 1, 1);
        encode_varint_field(&mut bytes, 3, 1);
        encode_varint_field(&mut bytes, 4, 1);
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "type");
        encode_string_field(&mut hdr, 2, "event");
        encode_bytes_field(&mut bytes, 5, &hdr);
        encode_bytes_field(&mut bytes, 8, br#"{"header":{"event_type":"im.message.receive_v1"}}"#);

        let frame = parse_frame(&bytes).unwrap();
        assert_eq!(frame.headers.get("type").map(|s| s.as_str()), Some("event"));

        let event_type = serde_json::from_slice::<serde_json::Value>(&frame.payload)
            .ok()
            .and_then(|v| v.get("header")?.get("event_type")?.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        assert_eq!(event_type, "im.message.receive_v1");
    }

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
        let event_id = event
            .header
            .as_ref()
            .and_then(|h| h.event_id.clone());
        assert_eq!(event_id.as_deref(), Some("e1"));
    }

    /// build_response_frame must echo the request's routing fields so Feishu
    /// can match the ack to the original card action.
    #[test]
    fn response_frame_echoes_request_fields() {
        let mut bytes = Vec::new();
        encode_varint_field(&mut bytes, 1, 99);
        encode_varint_field(&mut bytes, 2, 5);
        encode_varint_field(&mut bytes, 3, 12);
        encode_varint_field(&mut bytes, 4, 2);
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "type");
        encode_string_field(&mut hdr, 2, "card");
        encode_bytes_field(&mut bytes, 5, &hdr);
        encode_string_field(&mut bytes, 6, "json");
        encode_string_field(&mut bytes, 7, "event");
        encode_bytes_field(&mut bytes, 8, b"{}");
        encode_string_field(&mut bytes, 9, "log-new-9");

        let request = parse_frame(&bytes).unwrap();
        let resp = build_response_frame(&request, None).expect("response builds");

        let parsed = parse_frame(&resp).expect("response parses");
        assert_eq!(parsed.seq_id, 99);
        assert_eq!(parsed.log_id, 5);
        assert_eq!(parsed.service, 12);
        assert_eq!(parsed.method, 2);
        assert_eq!(parsed.log_id_new.as_deref(), Some("log-new-9"));
        // payload is a JSON string {"code":200,"headers":null,"data":"<b64>"}
        let payload_str = String::from_utf8_lossy(&parsed.payload);
        let v: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
        assert_eq!(v["code"], 200);
        assert!(v["data"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
        // headers echoed back (plus biz_rt)
        assert!(parsed.headers.len() >= 2);
    }

    /// Every WS event (message receive, bot added, ...) must be acked with a
    /// code-200 frame, exactly like the Lark SDK does for each event. Feishu
    /// re-delivers events it never sees an ack for, then stops pushing.
    #[test]
    fn event_ack_frame_has_code_200_and_null_data() {
        let mut bytes = Vec::new();
        encode_varint_field(&mut bytes, 1, 7);
        encode_varint_field(&mut bytes, 2, 3);
        encode_varint_field(&mut bytes, 3, 12);
        encode_varint_field(&mut bytes, 4, 1);
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "type");
        encode_string_field(&mut hdr, 2, "event");
        encode_bytes_field(&mut bytes, 5, &hdr);
        encode_bytes_field(
            &mut bytes,
            8,
            br#"{"header":{"event_type":"im.message.receive_v1"}}"#,
        );

        let request = parse_frame(&bytes).unwrap();
        let ack = build_event_ack_frame(&request).expect("ack builds");

        let parsed = parse_frame(&ack).expect("ack parses");
        assert_eq!(parsed.seq_id, 7);
        assert_eq!(parsed.service, 12);
        // payload must be {"code":200,"headers":null,"data":null}
        let payload_str = String::from_utf8_lossy(&parsed.payload);
        let v: serde_json::Value = serde_json::from_str(&payload_str).unwrap();
        assert_eq!(v["code"], 200);
        assert_eq!(v["data"], serde_json::Value::Null);
    }

    /// A pbbp2 control "ping" frame from Feishu must be answered with a
    /// "pong" frame echoing the request's routing fields; ignoring it lets
    /// the server consider the connection dead.
    #[test]
    fn ping_frame_produces_pong_frame() {
        let mut bytes = Vec::new();
        encode_varint_field(&mut bytes, 1, 11);
        encode_varint_field(&mut bytes, 3, 12);
        encode_varint_field(&mut bytes, 4, 0);
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, "type");
        encode_string_field(&mut hdr, 2, "ping");
        encode_bytes_field(&mut bytes, 5, &hdr);

        let request = parse_frame(&bytes).unwrap();
        let pong = build_pong_frame(&request).expect("pong builds");

        let parsed = parse_frame(&pong).expect("pong parses");
        assert_eq!(parsed.seq_id, 11);
        assert_eq!(parsed.service, 12);
        assert_eq!(parsed.headers.get("type").map(|s| s.as_str()), Some("pong"));
    }

    /// The keepalive ping cola sends itself (like the SDK's pingLoop) must be
    /// a control frame carrying headers type=ping.
    #[test]
    fn ping_frame_built_for_keepalive() {
        let ping = build_ping_frame();
        let parsed = parse_frame(&ping).expect("ping parses");
        assert_eq!(parsed.method, 0); // FrameTypeControl
        assert_eq!(parsed.headers.get("type").map(|s| s.as_str()), Some("ping"));
    }
}
