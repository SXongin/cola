//! The Feishu long-connection wire protocol (pbbp2): a hand-rolled protobuf
//! codec for the binary `Frame` message plus the one frame writer every reply
//! goes through. Pure bytes in / bytes out — no socket, no event semantics;
//! the connection lifecycle and dispatch live in the sibling `ws` module.

use std::collections::HashMap;

/// The routing fields of a frame, echoed back on every reply so the server
/// can match it to its request.
#[derive(Default)]
pub(crate) struct Routing {
    /// Echoed only when non-zero, like the Lark SDK does.
    pub(crate) seq_id: u64,
    /// Echoed only when non-zero.
    pub(crate) log_id: u64,
    pub(crate) service: i32,
    pub(crate) method: i32,
    pub(crate) payload_encoding: Option<String>,
    pub(crate) payload_type: Option<String>,
    pub(crate) log_id_new: Option<String>,
}

/// A decoded Feishu WS frame (protobuf Frame message).
pub(crate) struct Frame {
    pub(crate) routing: Routing,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) payload: Vec<u8>,
}

impl Frame {
    /// Parse a Feishu WS binary frame (protobuf Frame message).
    pub(crate) fn decode(data: &[u8]) -> Option<Frame> {
        let mut pos = 0;
        let mut frame = Frame {
            routing: Routing::default(),
            headers: HashMap::new(),
            payload: Vec::new(),
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
                    frame.routing.seq_id = v;
                }
                (2, 0) => {
                    // LogID (uint64)
                    let (v, p) = read_varint(data, pos)?;
                    pos = p;
                    frame.routing.log_id = v;
                }
                (3, 0) => {
                    // Service (int32)
                    let (v, p) = read_varint(data, pos)?;
                    pos = p;
                    frame.routing.service = v as i32;
                }
                (4, 0) => {
                    // Method (int32)
                    let (v, p) = read_varint(data, pos)?;
                    pos = p;
                    frame.routing.method = v as i32;
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
                    frame.routing.payload_encoding =
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
                    frame.routing.payload_type =
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
                    frame.routing.log_id_new =
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
}

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

/// Write one pbbp2 frame: the routing echo (fields 1-4, 6-7, 9), `headers` as
/// repeated field 5 in the given order, and `payload` in field 8. Header policy
/// stays with the caller: event replies echo the request's headers plus
/// `biz_rt`; control frames set their own `type`.
pub(crate) fn encode(routing: &Routing, headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();

    // Field 1: seq_id (varint)
    if routing.seq_id != 0 {
        encode_varint_field(&mut out, 1, routing.seq_id);
    }
    // Field 2: log_id (varint)
    if routing.log_id != 0 {
        encode_varint_field(&mut out, 2, routing.log_id);
    }
    // Field 3: service (varint)
    encode_varint_field(&mut out, 3, routing.service as u64);
    // Field 4: method (varint)
    encode_varint_field(&mut out, 4, routing.method as u64);

    // Field 5: headers (length-delimited, repeated)
    for (k, v) in headers {
        let mut hdr = Vec::new();
        encode_string_field(&mut hdr, 1, k);
        encode_string_field(&mut hdr, 2, v);
        encode_bytes_field(&mut out, 5, &hdr);
    }

    // Field 6: payload_encoding
    if let Some(pe) = &routing.payload_encoding {
        encode_string_field(&mut out, 6, pe);
    }
    // Field 7: payload_type
    if let Some(pt) = &routing.payload_type {
        encode_string_field(&mut out, 7, pt);
    }
    // Field 8: payload (bytes)
    encode_bytes_field(&mut out, 8, payload);
    // Field 9: log_id_new
    if let Some(lin) = &routing.log_id_new {
        encode_string_field(&mut out, 9, lin);
    }

    out
}

/// Answer a pbbp2 control "ping" frame with a "pong" so the server keeps the
/// connection alive. Echoes the request's routing fields; the headers type
/// becomes "pong".
pub(crate) fn pong(request: &Frame) -> Vec<u8> {
    encode(&request.routing, &[("type", "pong"), ("biz_rt", "1")], b"")
}

/// Build a keepalive ping frame cola sends proactively (mirroring the Lark
/// SDK's pingLoop) so an idle connection is detected as dead and reconnects.
pub(crate) fn ping() -> Vec<u8> {
    encode(&Routing::default(), &[("type", "ping")], b"")
}

fn encode_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    let tag = field << 3; // wire type 0 = varint
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a WS frame with the encoder, then confirm decode recovers every
    /// routing field, the headers and the payload.
    #[test]
    fn frame_round_trip() {
        let bytes = encode(
            &Routing {
                seq_id: 42,
                log_id: 7,
                service: 12,
                method: 3,
                payload_encoding: Some("json".to_string()),
                payload_type: Some("event".to_string()),
                log_id_new: Some("log-new-1".to_string()),
            },
            &[("type", "event")],
            br#"{"header":{"event_type":"im.message.receive_v1"}}"#,
        );

        let frame = Frame::decode(&bytes).expect("frame parses");
        assert_eq!(frame.routing.seq_id, 42);
        assert_eq!(frame.routing.log_id, 7);
        assert_eq!(frame.routing.service, 12);
        assert_eq!(frame.routing.method, 3);
        assert_eq!(frame.headers.get("type").map(|s| s.as_str()), Some("event"));
        assert_eq!(frame.routing.payload_encoding.as_deref(), Some("json"));
        assert_eq!(frame.routing.payload_type.as_deref(), Some("event"));
        assert_eq!(frame.routing.log_id_new.as_deref(), Some("log-new-1"));
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
        let bytes = encode(
            &Routing {
                seq_id: 1,
                service: 1,
                method: 1,
                ..Routing::default()
            },
            &[("type", "event")],
            br#"{"header":{"event_type":"im.message.receive_v1"}}"#,
        );

        let frame = Frame::decode(&bytes).unwrap();
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
        let request = Frame::decode(&encode(
            &Routing {
                seq_id: 11,
                service: 12,
                ..Routing::default()
            },
            &[("type", "ping")],
            b"",
        ))
        .unwrap();

        let pong = pong(&request);

        let parsed = Frame::decode(&pong).expect("pong parses");
        assert_eq!(parsed.routing.seq_id, 11);
        assert_eq!(parsed.routing.service, 12);
        assert_eq!(parsed.headers.get("type").map(|s| s.as_str()), Some("pong"));
    }

    /// The keepalive ping cola sends itself (like the SDK's pingLoop) must be
    /// a control frame carrying headers type=ping.
    #[test]
    fn ping_frame_built_for_keepalive() {
        let ping = ping();
        let parsed = Frame::decode(&ping).expect("ping parses");
        assert_eq!(parsed.routing.method, 0); // FrameTypeControl
        assert_eq!(parsed.headers.get("type").map(|s| s.as_str()), Some("ping"));
    }
}
