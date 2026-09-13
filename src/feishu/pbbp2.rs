//! The Feishu long-connection wire protocol (pbbp2): a hand-rolled protobuf
//! codec for the binary `Frame` message plus the frame writers cola replies
//! with. Pure bytes in / bytes out — no socket, no event semantics; the
//! connection lifecycle and dispatch live in the sibling `ws` module.

use std::collections::HashMap;

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
pub(crate) struct ParsedFrame {
    pub(crate) seq_id: u64,
    pub(crate) log_id: u64,
    pub(crate) service: i32,
    pub(crate) method: i32,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) payload_encoding: Option<String>,
    pub(crate) payload_type: Option<String>,
    pub(crate) payload: Vec<u8>,
    pub(crate) log_id_new: Option<String>,
}

/// Parse a Feishu WS binary frame (protobuf Frame message).
pub(crate) fn parse_frame(data: &[u8]) -> Option<ParsedFrame> {
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
                frame.log_id_new = Some(String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string());
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

/// Answer a pbbp2 control "ping" frame with a "pong" so the server keeps the
/// connection alive. Echoes routing fields; the headers type becomes "pong".
pub(crate) fn build_pong_frame(request: &ParsedFrame) -> Option<Vec<u8>> {
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
pub(crate) fn build_ping_frame() -> Vec<u8> {
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
pub(crate) fn encode_response_frame(request: &ParsedFrame, payload: &str) -> Vec<u8> {
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

pub(crate) fn encode_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    let tag = field << 3; // wire type 0 = varint
    encode_varint(out, tag as u64);
    encode_varint(out, value);
}

pub(crate) fn encode_string_field(out: &mut Vec<u8>, field: u32, value: &str) {
    encode_bytes_field(out, field, value.as_bytes());
}

pub(crate) fn encode_bytes_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        encode_bytes_field(
            &mut bytes,
            8,
            br#"{"header":{"event_type":"im.message.receive_v1"}}"#,
        );
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
        encode_bytes_field(
            &mut bytes,
            8,
            br#"{"header":{"event_type":"im.message.receive_v1"}}"#,
        );

        let frame = parse_frame(&bytes).unwrap();
        assert_eq!(frame.headers.get("type").map(|s| s.as_str()), Some("event"));

        let event_type = serde_json::from_slice::<serde_json::Value>(&frame.payload)
            .ok()
            .and_then(|v| {
                v.get("header")?
                    .get("event_type")?
                    .as_str()
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        assert_eq!(event_type, "im.message.receive_v1");
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
