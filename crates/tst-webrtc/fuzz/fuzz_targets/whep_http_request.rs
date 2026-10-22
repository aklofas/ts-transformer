#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_webrtc::server::http::{HeadParse, MAX_BODY, MAX_HEAD, parse_head, route};

fuzz_target!(|data: &[u8]| {
    // The parser never panics, and a complete head lies inside the input
    // and inside the head limit, with a body length inside the body limit.
    if let HeadParse::Done { request, head_len } = parse_head(data) {
        assert!(head_len <= data.len() && head_len <= MAX_HEAD);
        assert!(request.content_length <= MAX_BODY);
        let _ = route(&request);
    }
});
