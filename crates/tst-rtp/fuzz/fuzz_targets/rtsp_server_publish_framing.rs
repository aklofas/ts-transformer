#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_rtp::rtsp::message::{RtspFraming, rtsp_frame_decision};
use tst_rtp::rtsp::server::publish::drain_interleaved_head;

// Feed arbitrary bytes through the server session loop's framing for a
// publisher connection, where `$`-framed RTP/RTCP and RTSP requests
// interleave freely (RFC 2326 §10.12). `drain_interleaved_head` is the
// loop's own interleaved step and `rtsp_frame_decision` its request
// framing, both pure functions, so this exercises the server's real parsing
// path without a socket. A complete request is drained whole, as the loop
// does whether or not it parses; any reject or "need more" ends the input.
// The loop is capped at 100 rounds so a pathological seed cannot exhaust
// the wall-clock budget.
fuzz_target!(|data: &[u8]| {
    let mut buf = data.to_vec();
    let mut route = |_channel: u8, _payload: &[u8]| {};
    for _ in 0..100 {
        if !drain_interleaved_head(&mut buf, &mut route) {
            break;
        }
        match rtsp_frame_decision(&buf) {
            RtspFraming::Complete { total_len } => {
                buf.drain(..total_len);
            }
            RtspFraming::NeedMore
            | RtspFraming::HeadersTooLong
            | RtspFraming::BadContentLength(_) => break,
        }
    }
});
