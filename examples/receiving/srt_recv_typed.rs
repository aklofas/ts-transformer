//! Bind an SRT listener, accept one peer, dump the typed `DemuxEvent`
//! stream from `pipeline::DemuxReceiver`.
//!
//! Why this exists: the smallest end-to-end demonstration that
//! `pipeline::DemuxReceiver` turns "bytes in via SRT" into "typed events
//! out" with no intermediate demuxer plumbing visible to the caller.
//! `MuxSender` on the publishing side and `DemuxReceiver` on the consuming
//! side are deliberately mirror-image shapes — see
//! `send_pipeline_to_socket.rs` for the producer counterpart, and run
//! the two together for an end-to-end smoke.
//!
//! Usage:
//!   # terminal A (this example)
//!   cargo run -p tst-examples --example srt_recv_typed -- 9000
//!
//!   # terminal B (the publisher)
//!   cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000
//!
//! What to look for in the output:
//! - One `ProgramMap` line shortly after the peer connects (the demuxer
//!   needs the PAT + PMT before it can label streams; for a 100ms PSI
//!   cadence sender, ~100ms after first byte).
//! - `Sample` lines as video AUs arrive. The `pts` should advance by
//!   ~3000 ticks per frame at 30 fps (90 kHz / 30 = 3000).
//! - `Metadata` lines if the publisher sends KLV.
//! - Clean exit when the publisher closes — no error message, just
//!   "peer closed cleanly".

use std::env;
use std::time::Duration;
use tst_core::mpegts::demux::DemuxEvent;
use tst_pipeline::DemuxReceiver;
use tst_srt::ListenerBuilder;
use tst_srt::SrtTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = env::args()
        .nth(1)
        .unwrap_or_else(|| "9000".into())
        .parse()
        .expect("port must be u16");

    // Build the listener with a 120ms latency budget. SRT's `latency`
    // option is the wall-clock delay the receiver buffers before
    // releasing packets — long enough to absorb typical jitter and a
    // handful of retransmissions, short enough that interactive use
    // doesn't suffer. Both peers must agree on this value (libsrt
    // negotiates the maximum during handshake), so the matching
    // `send_pipeline_to_socket` example uses the same 120ms.
    //
    // `bind` is the terminal call on `ListenerBuilder` — no separate
    // `.build()` step. It returns a ready-to-accept `Listener`.
    //
    // Bind-then-step shape (`ListenerBuilder` is `&mut self -> &mut Self`):
    // construct, mutate, then call the terminal `bind`.
    let mut lb = ListenerBuilder::new();
    lb.latency(Duration::from_millis(120));
    let mut listener = lb.bind(format!("0.0.0.0:{port}").as_str())?;
    eprintln!("listening on 0.0.0.0:{port}");

    // Blocking until the first handshake completes. For a long-running
    // service you would loop on `accept` and spawn a thread per peer;
    // this example is single-shot.
    let (socket, peer) = listener.accept()?;
    eprintln!("peer connected: {peer}");

    // `SrtTransport::new` wraps the connected `Socket` so it satisfies
    // the `RecvTransport` trait that `DemuxReceiver` is generic over. The
    // same wrapper also satisfies `Transport` for the send side; one
    // socket, one wrapper, both directions.
    let mut rx = DemuxReceiver::new(SrtTransport::new(socket));

    // `DemuxReceiver` implements `Iterator<Item = Result<DemuxEvent, DemuxReceiverError>>`,
    // so `for result in &mut rx` is the idiomatic drain pattern.
    //
    // Stream-end contract:
    //
    //   - **Iterator termination** (the `for` loop simply ends) is the
    //     canonical clean-exit signal. It fires when the underlying
    //     `RecvTransport` returns `TransportError::Closed`, which
    //     `DemuxReceiver::recv_event` translates into `Ok(None)` after first
    //     calling `Demuxer::flush()` to recover any trailing PES. Loop
    //     callers do not see `Closed` as an error variant.
    //
    //   - **`Err(err)` with `err.kind == ShellErrorKind::TransportBroken`** fires
    //     for both peer-initiated cleanup and unrecoverable links.
    //     `SrtTransport` collapses these into one `Broken` surface by design —
    //     it lets a managed-receive decorator distinguish a self-initiated close
    //     (`Closed`) from a peer-initiated break (`Broken`). For a non-managed
    //     example like this one, both are terminal. On `Broken` the receiver
    //     flushes the demuxer first, so any trailing PES arrives as events
    //     before the `Err`.
    //     Inner source: `matches!(err.source, DemuxReceiverErrorSource::Transport(
    //     TransportError::Broken { .. }))` if you need variant discrimination.
    //
    //   - **`Err(err)` with `err.kind == ShellErrorKind::InputMalformed`** is a
    //     strict-mode rejection or malformed PES — fatal for this example.
    //     Inner source: `matches!(err.source, DemuxReceiverErrorSource::Demux(_))`.
    for item in &mut rx {
        match item {
            Ok(DemuxEvent::ProgramMap(m)) => {
                eprintln!(
                    "ProgramMap: program={} streams={} klv_links={}",
                    m.program_number,
                    m.streams.len(),
                    m.klv_links.len()
                );
            }
            Ok(DemuxEvent::Sample { stream, pts, .. }) => {
                eprintln!("Sample PID=0x{:04X} pts={}", stream.pid, pts.as_ticks());
            }
            Ok(DemuxEvent::Metadata {
                stream,
                pts,
                kind,
                payload,
            }) => {
                eprintln!(
                    "Metadata PID=0x{:04X} pts={} kind={kind:?} bytes={}",
                    stream.pid,
                    pts.as_ticks(),
                    payload.len()
                );
            }
            Ok(DemuxEvent::Discontinuity { stream, kind }) => {
                eprintln!("Discontinuity PID=0x{:04X} {kind:?}", stream.pid);
            }
            Ok(DemuxEvent::NonConformant { stream, issue }) => {
                eprintln!("NonConformant PID=0x{:04X} {issue:?}", stream.pid);
            }
            // Emitted only by the reconnect-aware `ManagedDemuxReceiver`
            // shell (which this example doesn't use); plain `DemuxReceiver`
            // never produces this. Included only to satisfy exhaustive
            // matching on `DemuxEvent`.
            Ok(DemuxEvent::ReconnectDiscontinuity) => {}
            // Any error variant terminates this single-shot example.
            // `err.kind == TransportBroken` fires on peer disconnect (clean
            // or not); `err.kind == InputMalformed` fires in strict mode or
            // on a malformed PES. Clean EOF is iterator termination, not an
            // `Err` arm.
            Err(e) => {
                eprintln!("receiver error: {e}");
                break;
            }
        }
    }
    eprintln!("peer disconnected");
    Ok(())
}
