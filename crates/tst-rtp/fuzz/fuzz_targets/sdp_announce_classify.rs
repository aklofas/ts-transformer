#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_rtp::Sdp;
use tst_rtp::rtsp::server::publish::classify_announce;

// Parse arbitrary bytes as an ANNOUNCE body and run the server's shape
// classification on whatever parses. Accepted shapes and rejections are
// both fine; the harness asserts no panics.
fuzz_target!(|data: &[u8]| {
    if let Ok(sdp) = Sdp::parse(data) {
        let _ = classify_announce(&sdp);
    }
});
