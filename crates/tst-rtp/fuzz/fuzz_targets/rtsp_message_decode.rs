#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes to RtspResponse::parse. Both successful parses
// (returning the message + consumed length) and parse errors are fine;
// the harness asserts no panics, no unsoundness, no unbounded memory.
fuzz_target!(|data: &[u8]| {
    let _ = tst_rtp::rtsp::message::RtspResponse::parse(data);
});
