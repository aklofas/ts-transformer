//! ManagedTransport demonstration with a deliberately flaky peer.
//!
//! Spawns a listener thread that accepts a connection, reads a few messages,
//! drops the connection, then re-accepts. The sender uses ManagedTransport
//! wrapping SrtTransport in the default `ReconnectMode::Blocking`: the send
//! that hits the break holds its one message, reconnects with exponential
//! backoff on the calling thread, delivers that message on the new link and
//! only then returns. Nothing accumulates while the link is down, because
//! the producer is parked inside that call. (The sibling
//! `managed_reconnect_background.rs` is the mode that queues an outage's
//! messages — and the place to read about sizing the gap buffer.)
//!
//!   cargo run -p tst-examples --example managed_reconnect
//!
//! Watch stderr for reconnect events.

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedTransport, MuxSender, OverflowPolicy, ReconnectPolicy,
    TransportError,
};
use tst_srt::SrtTransport;
use tst_srt::{ListenerBuilder, SocketBuilder};

// NUM_FRAMES is the total number of synthetic video frames the sender pushes.
// Sized so we span at least two peer-induced disconnects with comfortable
// headroom — at 30 fps this is ~1 second of "video" wall time. Small on
// purpose: this is a smoke test for the reconnect machinery, not a
// throughput demo.
//
// FRAMES_BEFORE_DROP is informational — it tracks the comment we print to
// stderr at the start of the run. The actual disconnect trigger lives on
// the peer side and counts *messages* (TS chunks), not video frames; one
// video frame produces multiple messages because the muxer fragments AUs
// into 1316-byte SRT payloads. Keeping the constants close avoids the two
// numbers drifting if a future tweak retunes the demo.
const NUM_FRAMES: usize = 30;
const FRAMES_BEFORE_DROP: usize = 10;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Pick a free ephemeral port via TCP-bind-on-0 / read / drop. Same idiom
    // as `encrypted_send_recv` — see that example for the longer rationale.
    // Briefly: SRT runs over UDP, so the temporarily-bound TCP socket
    // doesn't conflict with the SRT bind that follows; we're using TCP
    // only because its `local_addr()` after `bind(0)` is the canonical
    // way to ask the kernel for an unused port number.
    let port = {
        let l = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let p = l.local_addr()?.port();
        drop(l);
        p
    };
    let bind_addr = format!("127.0.0.1:{port}");
    let connect_addr = bind_addr.clone();

    // `listener_done` is the shutdown flag the main thread flips at the end
    // of the run to tell the peer thread "stop after the current accept."
    // An AtomicBool — not an mpsc — because this is a *flag* (one-shot,
    // boolean, no payload), and the peer thread polls it from inside its
    // own loop after each round. mpsc would imply a queue-with-payload
    // semantic we don't need; an atomic is the lighter primitive that
    // exactly matches the use.
    let listener_done = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<()>();

    // ---------------------------------------------------------------------
    // Flaky peer thread.
    //
    // This thread *simulates* a flaky receiver — its job is to deliberately
    // drop the connection partway through so the sender can demonstrate
    // reconnect behavior. Real receivers do not behave this way; they
    // accept once and drain. We spawn a peer that misbehaves on purpose
    // so the example exercises the failure-handling stack
    // (`ManagedTransport`, `ReconnectPolicy`, backoff, redelivery of the
    // interrupted message) end-to-end inside a single process, with no
    // external test harness.
    //
    // Pattern across rounds:
    //   round 0: accept → drain 5 messages → drop (induce disconnect #1)
    //   round 1: accept → drain 5 messages → drop (induce disconnect #2)
    //   round 2: accept → drain to clean close (let the sender finish)
    //
    // Two simulated outages followed by a clean tail. This is the minimum
    // shape that exercises the backoff-then-retry path, and the redelivery
    // of the message that found the break, more than once, while still
    // terminating in finite time.
    // ---------------------------------------------------------------------
    let peer_done = listener_done.clone();
    let peer_handle = thread::spawn(
        move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            // Latency must match the sender's (120 ms) — SRT negotiates the max
            // of the two peers' values, and a mismatch is a common config
            // smell to flag.
            //
            // Bind-then-step shape (`ListenerBuilder` is `&mut self -> &mut Self`).
            let mut lb = ListenerBuilder::new();
            lb.latency(Duration::from_millis(120));
            let mut listener = lb.bind(bind_addr.as_str())?;
            ready_tx.send(()).ok();

            for round in 0..3 {
                let (mut socket, peer) = listener.accept()?;
                eprintln!("peer: round {round} accepted from {peer}");

                // 1500 bytes ≥ default SRT payload (1316), so each `recv`
                // returns a whole message.
                let mut buf = [0u8; 1500];
                let mut messages = 0;
                loop {
                    match socket.recv(&mut buf) {
                        Ok(_) => {
                            messages += 1;
                            if messages >= 5 && round < 2 {
                                eprintln!("peer: round {round} dropping after {messages} messages");
                                // Dropping the `Socket` runs its `Drop` impl,
                                // which calls `srt_close` on the underlying
                                // libsrt handle. libsrt sends a teardown to the
                                // remote peer; the sender sees that as a
                                // broken connection on its next `send`, which
                                // surfaces as `TransportError::Broken`.
                                // *That* is what triggers `ManagedTransport`'s
                                // reconnect path. So this single line
                                // `drop(socket)` is the entire disconnect
                                // simulation.
                                drop(socket);
                                break;
                            }
                        }
                        // Canonical clean-close signal — the sender called
                        // `close()` and we should exit the recv loop.
                        Err(tst_srt::error::RecvError::ConnectionBroken) => {
                            eprintln!("peer: round {round} clean close after {messages} messages");
                            break;
                        }
                        // No recv timeout is configured, so this branch is
                        // defensive. Continue and try again.
                        Err(tst_srt::error::RecvError::TimedOut) => continue,
                        Err(e) => return Err(Box::new(e)),
                    }
                }
                // Honor coordinated shutdown — main flips this flag once the
                // sender has finished, so we don't loop into a 4th `accept()`
                // that would never complete.
                if peer_done.load(Ordering::SeqCst) {
                    break;
                }
            }
            Ok(())
        },
    );

    // Wait for `bind()` to return on the peer thread, then a small extra
    // pause to let the kernel start servicing UDP on the listening socket
    // before our first handshake datagram lands.
    ready_rx.recv()?;
    thread::sleep(Duration::from_millis(50));

    // ---------------------------------------------------------------------
    // Factory closure — what `ManagedTransport` calls to rebuild the inner
    // transport after each disconnect.
    //
    // Trait bounds: `Fn` (callable many times — once per reconnect),
    // `Send + Sync + 'static` (so `ManagedTransport` can store it in an
    // `Arc<dyn Fn ...>` and potentially call it from a background thread).
    // The `move` captures `connect_addr_for_factory` by value — the closure
    // owns its address string, so each rebuild knows where to dial.
    //
    // We map `ConnectError` (the rich, typed error from `SocketBuilder`)
    // onto `TransportError::Broken` because `ManagedTransport`'s contract
    // speaks `TransportError`. The projection collapses a number of
    // distinct connect-time failure modes into one bucket — that's
    // intentional; from the reconnect-loop's perspective they all mean
    // the same thing: "we couldn't establish the link, back off and try
    // again."
    // ---------------------------------------------------------------------
    let connect_addr_for_factory = connect_addr.clone();
    let factory = move || -> Result<SrtTransport, TransportError> {
        // Bind-then-step shape (`SocketBuilder` is `&mut self -> &mut Self`).
        // `connect` is a `&self` terminal so chaining `.map_err()` after it on
        // the same expression is fine — the named binding only matters for the
        // mutating steps.
        let mut sb = SocketBuilder::new();
        sb.latency(Duration::from_millis(120));
        let socket =
            sb.connect(connect_addr_for_factory.as_str())
                .map_err(|e| TransportError::Broken {
                    msg: format!("connect failed: {e}"),
                    // Examples don't propagate the typed libsrt errno here —
                    // the educational point is the reconnect shape, not the
                    // typed-source plumbing. Real consumers wrapping
                    // `Socket::connect` directly should fish out the typed
                    // error and map it to a meaningful code.
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })?;
        Ok(SrtTransport::new(socket))
    };

    // First connect runs synchronously — if even the *initial* link fails
    // there's no point spinning up the rest of the pipeline. Subsequent
    // failures are absorbed by `ManagedTransport`.
    let initial = factory().map_err(|e| format!("initial connect: {e:?}"))?;

    // ---------------------------------------------------------------------
    // ReconnectPolicy — the four knobs set here, plus `mode`, which is left
    // at its default (`ReconnectMode::Blocking`) by `..Default::default()`.
    // Two of the four do their real work only in the OTHER mode; they are
    // spelled out anyway so the literal shows the whole policy.
    //
    //   max_attempts: Some(20)
    //     Up to 20 reconnect attempts per outage. When they run out, the
    //     `send_*` call that was waiting returns an error wrapping
    //     `TransportError::Broken` ("reconnect gave up after 20 attempts").
    //     That is not terminal: the wrapper does not latch closed, and the
    //     next send starts a fresh cycle with a fresh budget. `None` would
    //     mean retry forever; `Some(20)` is bounded so this example
    //     terminates even if the peer thread crashes (defensive).
    //
    //   backoff: Exponential { base: 50ms, max: 2s }
    //     wait = 50ms * 2^(attempt-1), capped at 2s. Tuning rationale: the
    //     base is short so the demo iterates visibly fast; production
    //     defaults are 100 ms / 10 s. The cap prevents pathologically long
    //     waits if the peer stays down.
    //
    //   gap_buffer_capacity: 256
    //     The production default, and in THIS mode not a sizing decision.
    //     In `Blocking` mode the gap buffer holds exactly one message: the
    //     one whose send found the link broken. It goes in, the reconnect
    //     runs on this thread, the message is delivered on the new link,
    //     and the buffer is empty again when the call returns — or, if the
    //     call returns an error, the message is taken back out and is the
    //     caller's to resend. Nothing else can arrive meanwhile, because
    //     the only producer is the thread parked in that call. So do NOT
    //     size this as `outage duration × send rate` here: there is no
    //     backlog to hold, and a longer outage just means a longer block.
    //     The only value that changes `Blocking` behavior is 0 (buffer
    //     nothing, so even the interrupted message is dropped or refused).
    //     The `outage × rate` rule belongs to `ReconnectMode::Background`,
    //     where sends keep arriving during the outage — see
    //     `managed_reconnect_background.rs`.
    //
    //   overflow_policy: DropOldest
    //     What a FULL gap buffer does with a new message: evict the oldest
    //     (`DropOldest`) or refuse the new one (`Reject`). With one message
    //     in a 256-slot buffer it never fills, so in `Blocking` mode this
    //     knob is inert too (again except at capacity 0). The
    //     freshness-versus-completeness trade it expresses — after an
    //     outage, would the receiver rather have the newest frames or
    //     every frame? — is a `Background`-mode decision. What `Blocking`
    //     trades instead is time: no message is dropped by the wrapper,
    //     and the producer pays for that by stalling for the whole
    //     outage. Fine for a file or batch sender; wrong for a thread that
    //     is also draining a live encoder.
    // ---------------------------------------------------------------------
    let policy = ReconnectPolicy {
        max_attempts: Some(20),
        backoff: BackoffStrategy::Exponential {
            base: Duration::from_millis(50),
            max: Duration::from_secs(2),
        },
        gap_buffer_capacity: 256,
        overflow_policy: OverflowPolicy::DropOldest,
        ..Default::default()
    };
    let managed = ManagedTransport::new(initial, factory, policy);

    // The canonical sender shell: `MuxSender` composes the muxer
    // (`MuxerConfig::default`) with the transport. End-to-end the path is
    // NAL+KLV → mux → 188-byte TS packets → ManagedTransport → SrtTransport
    // → libsrt → wire. The `ManagedTransport` decorator is invisible to
    // `MuxSender` — it just sees a `Transport` impl that occasionally pauses
    // (for the length of a reconnect) and does not fail for an outage the
    // retry budget covers.
    let sender = MuxSender::new(managed, MuxerConfig::default())?;

    eprintln!("sender: sending {NUM_FRAMES} frames; peer drops after {FRAMES_BEFORE_DROP}");
    let mut sent_ok = 0usize;
    let mut sent_err = 0usize;
    for i in 0..NUM_FRAMES {
        // 90 kHz TS clock. 90000 Hz / 30 fps = 3000 ticks per frame, so
        // `i * 3000` advances PTS at exactly 30 fps cadence.
        let pts = (i as i64) * 3000;
        let nal = synthetic_nal_au(800);
        let klv = synthetic_klv(64, i as i64);
        // `key_frame: i == 0` — the first frame is the IDR; subsequent
        // frames are non-IDR. The synthetic NAL is tagged accordingly
        // (see `synthetic_nal_au`).
        match sender.send_video(&nal, Pts90khz::new(pts), i == 0) {
            Ok(()) => sent_ok += 1,
            Err(e) => {
                // Errors here are *informational*, not fatal. The
                // `ManagedTransport` decorator absorbs a broken link
                // inside the call (reconnects, then delivers this call's
                // bytes) and returns Ok — the outage shows up as a slow
                // call, not as an error. What does bubble up is a
                // reconnect that gave up (`Broken`, max_attempts
                // exhausted), backpressure from the fresh link, or an
                // oversized payload. In each of those the wrapper has
                // kept nothing: `MuxSender` retains the chunk it was
                // refused and offers it again on the next call. A retry
                // after an exhausted budget starts a fresh reconnect
                // cycle; a retry after Backpressure reuses the live link;
                // an oversized payload is rejected without touching the
                // link. So we log and keep going; the very next `send_*`
                // may well succeed.
                eprintln!("sender: send_video {i} -> {e:?}");
                sent_err += 1;
            }
        }
        // `metadata_service_id` goes into the AU cell header per H.222.0
        // §2.12.4.2 / ST 1402.2 App. B Table 2 for SynchronousMetadata
        // streams (stream_type 0x15); silently ignored for PrivateData
        // streams (0x06) like the one used here. The spec default is 0x00.
        match sender.send_klv(&klv, Pts90khz::new(pts), 0x00) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("sender: send_klv {i} -> {e:?}");
                sent_err += 1;
            }
        }
        // 33 ms ≈ 30 fps cadence. A real publisher would push frames as
        // soon as the encoder produces them and let SRT's pacing layer
        // shape the wire rate — application-layer pacing is not the
        // right place to do it.
        thread::sleep(Duration::from_millis(33));
    }
    eprintln!("sender: {sent_ok} OK, {sent_err} errored across reconnects");
    sender.close();

    // Coordinated shutdown: tell the peer thread to stop after its current
    // round, then join it. The `let _ =` discards `join`'s `Result` —
    // the peer thread can fail benignly if the listener was mid-`accept()`
    // when we closed (e.g. if it was about to accept a 4th round that the
    // sender will never make). For a smoke-test example we don't care.
    listener_done.store(true, Ordering::SeqCst);
    let _ = peer_handle.join();
    println!("OK: completed run with reconnects (sent_ok={sent_ok}, sent_err={sent_err})");
    Ok(())
}

// Synthetic H.264 access unit. The muxer doesn't parse NAL contents — it
// just wraps whatever bytes you give it in PES packets — but a real-looking
// AU helps if you tcpdump the output and load it in a tool that *does* parse.
//
// Layout:
//   0x00 0x00 0x00 0x01   Annex-B 4-byte start code
//   0x65                  NAL header byte for nal_unit_type=5 (IDR /
//                         coded slice of an IDR picture) with
//                         nal_ref_idc=0b11 (highest priority)
//   0xAA × n              filler payload (arbitrary; the muxer is
//                         opaque to NAL contents)
fn synthetic_nal_au(n: usize) -> Vec<u8> {
    let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    buf.extend(std::iter::repeat(0xAA).take(n));
    buf
}

// Synthetic KLV blob. The 16-byte prefix is the canonical ST 0601 UAS
// Datalink LS key (`UniversalLabel::ST_0601_LS`, MISB ST 0601 §6) —
// fine to hardcode here because the muxer is opaque to KLV contents
// (no parsing on the send path); it just wraps the blob in a
// metadata-stream PES and emits it. Real ST 0601 KLV is built via
// `tst_core::klv::st0601` (see the `klv_encode_minimal` example).
//
// `buf.push(n as u8)` is a BER short-form length byte, valid for n < 128
// (the high bit reserved for long-form indicator). This example keeps n
// well under that bound.
fn synthetic_klv(n: usize, seq: i64) -> Vec<u8> {
    let mut buf = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ];
    buf.push(n as u8);
    buf.extend(std::iter::repeat(seq as u8).take(n));
    buf
}
