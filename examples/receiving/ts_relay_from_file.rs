//! ts_relay_from_file — read pre-muxed `.ts` from disk and relay over SRT.
//!
//! Useful for testing SRT options against a captured stream without standing
//! up a live encoder. The pre-muxed-bytes path uses `pipeline::Sender`
//! (TS-bytes-in, transport-out) rather than `MuxSender` — the muxing already
//! happened upstream (e.g. by ffmpeg writing the input file).
//!
//! Pair with the file produced by `mux_to_file.rs` to round-trip the example
//! suite, or with any `.ts` file ffmpeg / GStreamer / a camera produced.
//!
//! On the receiver side, run something like:
//!   srt-live-transmit srt://:9000 file://con > /tmp/out.ts
//!
//! Usage: `cargo run -p tst-examples --example ts_relay_from_file -- input.ts 127.0.0.1:9000`

use std::env;
use std::fs::File;
use std::io::Read;
use std::time::Duration;
use tst_pipeline::{Sender, SenderConfig};
use tst_srt::SrtSocketBuilder;
use tst_srt::SrtTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: ts_relay_from_file <file.ts> <addr>")?;
    let addr = args.next().ok_or("missing addr")?;

    // 120 ms latency — reasonable LAN/regional WAN default. See
    // `send_pipeline_to_socket.rs` header for the full SrtSocketBuilder knob
    // discussion.
    //
    // Bind-then-step shape (`SrtSocketBuilder` is `&mut self -> &mut Self`):
    // construct, mutate, then call the terminal `connect`.
    let mut sb = SrtSocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let socket = sb.connect(addr.as_str())?;
    let transport = SrtTransport::new(socket);

    // `Sender` (vs `MuxSender`) accepts already-muxed TS bytes via `send_ts`.
    // It frames them into transport-sized payloads (default = 1316 = 7 TS
    // packets) and writes via the `Transport`. No muxer involved — the input
    // file already contains 188-byte TS packets.
    let mut sender = Sender::new(transport, SenderConfig::default());

    let mut file = File::open(&path)?;
    // 4 KiB chunks chosen as a sensible read granularity. The framer inside
    // `Sender` will re-chunk into 1316-byte payloads regardless, so this size
    // doesn't affect what goes on the wire — only how much we hold in memory
    // between syscalls.
    let mut buf = vec![0u8; 4096];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        // `send_ts` requires the buffer to be a contiguous run of 188-byte TS
        // packets. Since `File::read` returns arbitrary byte boundaries, this
        // works as long as the input file's length is a multiple of 188 (true
        // for any well-formed `.ts` file produced by ffmpeg, GStreamer, or
        // our own muxer). Mid-packet reads would be detected by `Sender`'s
        // framing state machine and surfaced as a `SendError`.
        sender.send_ts(&buf[..n])?;
    }
    // Flush any final partial chunk held in the framer.
    sender.flush()?;
    eprintln!("relayed {path} -> {addr}");
    // Stats include packets sent, bytes sent, and any underlying SRT error
    // counters. Useful for verifying the relay actually completed without
    // silent drops (e.g. due to peer disconnect mid-stream).
    eprintln!("stats: {:?}", sender.stats());
    // Cancel-first close (see `send_pipeline_to_socket.rs` for rationale).
    sender.close();
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}
