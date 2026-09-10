#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_rtp::rtsp::client::interleaved_pump::{
    RtspFrameBoundary, parse_binary_frame_header, scan_rtsp_message_boundary,
};

// Feed arbitrary bytes through the same `$<ch><len>` / CRLFCRLF+Content-
// Length framing rules `spawn_client_pump`'s inner loop uses on the wire
// (RFC 7826 §14). `parse_binary_frame_header` / `scan_rtsp_message_boundary`
// are `#[doc(hidden)] pub` extractions of that loop's boundary-detection
// step — pure functions, no socket/thread/channel required — so this
// exercises the client's real parsing path rather than a standalone
// reimplementation. We cap the loop at 100 frames so a pathological seed
// (e.g. zero-length binary frames) can't exhaust memory or wall-clock
// budget.
fuzz_target!(|data: &[u8]| {
    let mut buf = data.to_vec();
    for _ in 0..100 {
        if buf.is_empty() {
            break;
        }
        if buf[0] == b'$' {
            match parse_binary_frame_header(&buf) {
                Some((_channel, total_len)) => buf.drain(..total_len),
                None => break,
            };
        } else {
            match scan_rtsp_message_boundary(&buf) {
                RtspFrameBoundary::Incomplete => break,
                RtspFrameBoundary::NonUtf8Headers { skip } => {
                    buf.drain(..skip);
                }
                RtspFrameBoundary::BadContentLength { .. } | RtspFrameBoundary::LengthOverflow => {
                    break;
                }
                RtspFrameBoundary::Complete { len } => {
                    buf.drain(..len);
                }
            }
        }
    }
});
