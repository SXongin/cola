#![no_main]

use libfuzzer_sys::fuzz_target;

// `src/feishu/pbbp2.rs` is a private module of the binary crate, so the target
// compiles the file directly instead of linking the crate; its `pub(crate)`
// items are visible inside this fuzz crate.
#[allow(dead_code)]
#[path = "../../src/feishu/pbbp2.rs"]
mod pbbp2;

fuzz_target!(|data: &[u8]| {
    let Some(frame) = pbbp2::Frame::decode(data) else {
        return;
    };

    // decode -> encode -> decode must preserve everything the first decode
    // kept: the routing echo, every header, and the payload bytes.
    let headers: Vec<(&str, &str)> = frame
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let encoded = pbbp2::encode(&frame.routing, &headers, &frame.payload);
    let round = pbbp2::Frame::decode(&encoded).expect("encoded frame must decode");

    assert_eq!(round.payload, frame.payload);
    assert_eq!(round.headers, frame.headers);
    assert_eq!(round.routing.seq_id, frame.routing.seq_id);
    assert_eq!(round.routing.log_id, frame.routing.log_id);
    assert_eq!(round.routing.service, frame.routing.service);
    assert_eq!(round.routing.method, frame.routing.method);
    assert_eq!(round.routing.payload_encoding, frame.routing.payload_encoding);
    assert_eq!(round.routing.payload_type, frame.routing.payload_type);
    assert_eq!(round.routing.log_id_new, frame.routing.log_id_new);
});
