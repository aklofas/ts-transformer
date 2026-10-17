//! Minimal SRT receiver: bind, accept one connection, write bytes to a file.
//!
//! The peer for the existing `send_pipeline_to_socket` and `ts_relay_from_file`
//! examples — run this in one terminal, run the sender in another:
//!
//!   # terminal A (this example)
//!   cargo run -p tst-examples --example srt_listener_to_file -- 127.0.0.1:9000 out.ts
//!
//!   # terminal B (the sender)
//!   cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000
//!
//! Stops after the first sender disconnects.

use std::env;
use std::fs::File;
use std::io::Write;
use std::time::Duration;
use tst_srt::SrtListenerBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let bind_addr = args.next().unwrap_or_else(|| "127.0.0.1:9000".into());
    let out_path = args.next().unwrap_or_else(|| "out.ts".into());

    // Build the listener via the typed builder rather than constructing a
    // `ListenerConfig` directly — the builder validates each option as it is
    // set and produces a bound listener in a single call. The `latency(120ms)`
    // value is the conventional starting point for live SRT: enough buffer
    // for typical round-trip jitter and a handful of retransmissions, without
    // adding so much wall-clock delay that interactive use suffers. Both
    // peers must agree on the latency budget — see `send_pipeline_to_socket`,
    // which uses the same value.
    //
    // Bind-then-step shape (`SrtListenerBuilder` is `&mut self -> &mut Self`):
    // construct, mutate, then call the terminal `bind`.
    let mut lb = SrtListenerBuilder::new();
    lb.latency(Duration::from_millis(120));
    let mut listener = lb.bind(bind_addr.as_str())?;

    eprintln!("listening on {bind_addr}, writing to {out_path}");
    // Blocking call. Returns once the first peer's handshake completes (or, if
    // an `accept_timeout` were configured on the builder, once that fires).
    // For this single-shot example we want the block.
    let (mut socket, peer) = listener.accept()?;
    eprintln!("accepted from {peer}");

    let mut out = File::create(&out_path)?;
    // 1500 bytes is comfortably above the SRT default payload size of 1316
    // (`SRTO_PAYLOADSIZE`), so each `recv` returns one whole message that
    // fits in the buffer with room to spare. SRT is message-oriented in
    // live mode — partial-message reads aren't a concern.
    let mut buf = [0u8; 1500];
    let mut total: u64 = 0;
    loop {
        // Three-arm pattern covering the cases this example actually cares
        // about. Anything else is unexpected and propagates upward.
        match socket.recv(&mut buf) {
            // Normal data path: write through to the file and accumulate.
            Ok(n) => {
                out.write_all(&buf[..n])?;
                total += n as u64;
                // Heuristic "log every ~256 KiB" progress line. Works because
                // each `recv` returns one message bounded by the payload size
                // (≤ ~1316 bytes), so `n` is always far smaller than 256 KiB
                // and the modulo wraps cleanly through the threshold once per
                // ~256 KiB window.
                if total % (256 * 1024) < n as u64 {
                    eprintln!("received {total} bytes");
                }
            }
            // Peer closed the connection cleanly, or libsrt has decided the
            // link is unrecoverable. Either way this is the canonical
            // "we're done" signal — break and let main return Ok.
            Err(tst_srt::error::RecvError::ConnectionBroken) => {
                eprintln!("peer closed; received {total} bytes total");
                break;
            }
            // `TimedOut` only fires when a non-zero `SRTO_RCVTIMEO` has been
            // set. We don't set one, so this arm is defensive future-proofing
            // — keep looping if it ever does fire.
            Err(tst_srt::error::RecvError::TimedOut) => continue,
            // Anything else (auth failure, resource error, ...) is fatal for
            // this minimal example.
            Err(e) => return Err(Box::new(e)),
        }
    }
    Ok(())
}
