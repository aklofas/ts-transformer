#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes to the RTSP client engine as if read from the control
// connection: `$<ch><len>` interleaved frames (RFC 7826 §14), RTSP replies
// framed on CRLFCRLF + Content-Length, the header and body caps, and the
// replies' path into the request and keepalive state. The engine has no
// socket, thread or clock, so this drives the client's real input path.
// `fuzz_client_engine_input` feeds the bytes twice, in 7-byte slices and
// whole, to an engine with interleaved channels 0/1 and one OPTIONS waiting
// for its reply.
fuzz_target!(|data: &[u8]| {
    tst_rtp::rtsp::client::fuzz_client_engine_input(data);
});
