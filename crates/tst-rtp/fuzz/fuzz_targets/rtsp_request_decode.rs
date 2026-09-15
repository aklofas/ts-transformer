#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes to RtspRequest::parse — the SERVER-side request
// parser, fed straight from an unauthenticated TCP peer by the session
// loop (rtsp/server/session.rs). Sibling of rtsp_message_decode (the
// client-side RtspResponse::parse). Both successful parses and parse
// errors are fine; the harness asserts no panics, no unsoundness, no
// unbounded memory (Content-Length is capped at parse time, so a hostile
// declared body can never drive an allocation past MAX_RTSP_BODY_BYTES).
fuzz_target!(|data: &[u8]| {
    let _ = tst_rtp::RtspRequest::parse(data);
});
