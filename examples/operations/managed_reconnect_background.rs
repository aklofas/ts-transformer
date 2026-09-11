//! ManagedTransport demonstration of `ReconnectMode::Background`.
//!
//! Sibling to `managed_reconnect.rs` (which uses the default
//! `ReconnectMode::Blocking`) — same flaky-peer shape, same `SrtTransport`,
//! same `ManagedTransport` machinery. The only thing this example changes
//! is `policy.mode`, and that one field changes everything about how the
//! producer thread experiences an outage:
//!
//!   Blocking (the default): a `send_*` call that hits a broken transport
//!   blocks the caller until reconnect succeeds or `max_attempts` runs out.
//!   Background: a dedicated worker thread owns the factory/backoff/drain
//!   loop; `send_*` never waits on backoff or a factory call — it
//!   enqueues into the gap buffer and returns, whether or not the link
//!   is currently up. (It can still block briefly on internal lock
//!   contention while the worker is mid-drain — bounded to at most one
//!   in-flight inner send.)
//!
//!   cargo run -p tst-examples --example managed_reconnect_background
//!
//! Watch stderr: the send loop keeps producing at a steady cadence straight
//! through the outage (no stall), while a separate stats line shows the
//! gap buffer filling, messages being evicted, and the worker recovering.

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedTransport, MuxSender, OverflowPolicy, ReconnectMode,
    ReconnectPolicy, TransportError,
};
use tst_srt::SrtTransport;
use tst_srt::{ListenerBuilder, SocketBuilder};

// NUM_FRAMES / FRAME_INTERVAL together decide how many messages land while
// the link is down (see the peer thread below, and the factory-closure
// comment for why "down" often runs noticeably longer than the peer's raw
// ~300ms unreachable window). At one video frame + one KLV frame per
// iteration and a 10ms cadence, a run comfortably produces enough messages
// during the outage to overflow `gap_buffer_capacity` (4, chosen
// deliberately small below) many times over, so the demo reliably shows
// `DropOldest` evicting messages rather than leaving that to timing luck.
const NUM_FRAMES: usize = 120;
const FRAME_INTERVAL: Duration = Duration::from_millis(10);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Pick a free ephemeral port via TCP-bind-on-0 / read / drop. Same idiom
    // as `managed_reconnect.rs` — see that example for the longer rationale.
    let port = {
        let l = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let p = l.local_addr()?.port();
        drop(l);
        p
    };
    let bind_addr = format!("127.0.0.1:{port}");
    let connect_addr = bind_addr.clone();

    let (ready_tx, ready_rx) = mpsc::channel::<()>();

    // ---------------------------------------------------------------------
    // Flaky peer thread.
    //
    // Two rounds, and — unlike `managed_reconnect.rs`'s three-round peer —
    // this one actually *unbinds the listener* between rounds instead of
    // just closing the accepted socket. That distinction matters for this
    // demo: closing only the socket leaves the port bound and accepting, so
    // a reconnecting client could complete a new handshake almost
    // instantly, and the whole point of this example is to show
    // `send_bytes` staying decoupled from the reconnect backoff across a
    // *real*, sustained outage where the sink is genuinely unreachable —
    // not just a fast blip.
    //
    //   round 0: accept -> drain 5 messages -> drop socket -> drop listener
    //            (releases the port) -> sleep ~300ms (the sender's factory
    //            call blocks in-flight for the duration of the outage —
    //            see the factory closure comment below for why)
    //   round 1: rebind -> accept -> drain to clean close (let the sender
    //            finish)
    // ---------------------------------------------------------------------
    let peer_handle = thread::spawn(
        move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            for round in 0..2 {
                // Bind-then-step shape (`ListenerBuilder` is
                // `&mut self -> &mut Self`). Latency must match the
                // sender's (120 ms) — SRT negotiates the max of the two
                // peers' values.
                let mut lb = ListenerBuilder::new();
                lb.latency(Duration::from_millis(120));
                let mut listener = lb.bind(bind_addr.as_str())?;
                if round == 0 {
                    ready_tx.send(()).ok();
                }

                let (mut socket, peer) = listener.accept()?;
                eprintln!("peer: round {round} accepted from {peer}");

                // 1500 bytes >= default SRT payload (1316), so each `recv`
                // returns a whole message.
                let mut buf = [0u8; 1500];
                let mut messages = 0;
                loop {
                    match socket.recv(&mut buf) {
                        Ok(_) => {
                            messages += 1;
                            if messages >= 5 && round == 0 {
                                eprintln!("peer: round {round} dropping after {messages} messages");
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
                        Err(tst_srt::error::RecvError::TimedOut) => continue,
                        Err(e) => return Err(Box::new(e)),
                    }
                }

                // Drop the listener itself (not just the socket) so the
                // port stops accepting entirely — this is what turns the
                // gap into a real, sustained outage instead of a one-frame
                // blip.
                drop(listener);
                if round == 0 {
                    thread::sleep(Duration::from_millis(300));
                }
            }
            Ok(())
        },
    );

    // Wait for the first `bind()` to return, then a small extra pause to
    // let the kernel start servicing UDP on the listening socket before our
    // first handshake datagram lands.
    ready_rx.recv()?;
    thread::sleep(Duration::from_millis(50));

    // ---------------------------------------------------------------------
    // Factory closure — what `ManagedTransport` calls to rebuild the inner
    // transport after each disconnect. Under `Background` mode this runs
    // on the worker thread, not the caller's — see the policy comment
    // below for what that buys us.
    //
    // Note on what "one attempt" means here: SRT's own connect handshake
    // retries internally while waiting for a listener to respond, so a
    // single `factory()` call started while the peer is unreachable can
    // itself stay in flight for the whole outage — it doesn't fail fast
    // the way a TCP connect to a closed port would. That's why this demo
    // typically shows `reconnect_attempts` sitting at 1 for the entire
    // gap: `ReconnectPolicy.backoff` governs the pause *between* factory
    // calls, not how long any single call is allowed to hang, and here the
    // first call simply doesn't return (success or failure) until the
    // peer comes back. A production factory that wants
    // `ReconnectPolicy.backoff` to visibly drive multiple short attempts
    // would give the builder its own short `connect_timeout` so each
    // attempt fails fast instead of blocking on SRT's handshake retry.
    // ---------------------------------------------------------------------
    let connect_addr_for_factory = connect_addr.clone();
    let factory = move || -> Result<SrtTransport, TransportError> {
        let mut sb = SocketBuilder::new();
        sb.latency(Duration::from_millis(120));
        let socket =
            sb.connect(connect_addr_for_factory.as_str())
                .map_err(|e| TransportError::Broken {
                    msg: format!("connect failed: {e}"),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })?;
        Ok(SrtTransport::new(socket))
    };

    // First connect runs synchronously, same as `managed_reconnect.rs` —
    // `Background` mode only changes how *subsequent* reconnects behave.
    let initial = factory().map_err(|e| format!("initial connect: {e:?}"))?;

    // ---------------------------------------------------------------------
    // ReconnectPolicy — same four knobs as `managed_reconnect.rs`, plus
    // `mode`, which is the entire point of this example.
    //
    //   mode: ReconnectMode::Background
    //     Reconnect runs on a dedicated worker thread instead of the
    //     caller's. `send_video`/`send_klv` (which call down to
    //     `send_bytes` on the wrapped transport) never wait for the link
    //     to come back — while the worker is reconnecting they enqueue
    //     into the gap buffer under `overflow_policy` and return (a send
    //     can still block briefly on lock contention while the worker is
    //     mid-drain, bounded to one in-flight inner send). Reach for
    //     this in a single-threaded relay pump — one thread both
    //     produces frames and calls `send_bytes` — where
    //     blocking that thread through a whole reconnect window means the
    //     upstream source backs up or drops frames on the floor anyway.
    //     Prefer the default `Blocking` instead for batch/file senders,
    //     where losing bytes is worse than blocking and there is no
    //     time-sensitive upstream producer to protect — a blocked caller
    //     there just means "the batch job takes a bit longer," which is a
    //     fine trade for guaranteed in-order delivery.
    //
    //   max_attempts: Some(20)
    //     Bounds *one continuous outage* — the budget resets after every
    //     successful reconnect, exactly as it does in `Blocking` mode.
    //     If the background worker exhausts it (or the worker itself
    //     terminates abnormally, e.g. a panic), the *next* `send_*` call
    //     reports that failure exactly once as `TransportError::Broken`
    //     — "reconnect gave up after N attempts" or "background reconnect
    //     aborted (worker terminated abnormally)" — instead of the usual
    //     `Ok(())`. That call's own bytes are not queued; the caller owns
    //     the resend decision. This demo's outage is short enough that 20
    //     attempts is never exhausted, but production code polling
    //     `stats_handle()` should watch `reconnect_attempts` climbing
    //     toward this ceiling as an early warning sign.
    //
    //   backoff: Exponential { base: 50ms, max: 500ms }
    //     Governs the pause *between separate* `factory()` calls — short
    //     here so a fast-failing attempt would retry quickly. In this
    //     demo's particular outage a single attempt tends to absorb the
    //     whole gap on its own (see the factory closure comment above for
    //     why), so `reconnect_attempts` often reads 1 for the entire
    //     printout below rather than climbing — that is expected, not a
    //     bug in the backoff logic.
    //
    //   gap_buffer_capacity: 4
    //     Deliberately tiny — real deployments default to 256 (see
    //     `managed_reconnect.rs`). A capacity this small guarantees the
    //     many messages produced while the link is down overflow it many
    //     times over, so the stats printout below reliably shows
    //     `DropOldest` evicting messages instead of leaving that to timing
    //     luck.
    //
    //   overflow_policy: OverflowPolicy::DropOldest
    //     When the gap buffer is full, evict the oldest queued message and
    //     accept the new one. This is where "Ok(()) != delivered" comes
    //     from: `send_video`/`send_klv` still return `Ok(())` for a
    //     message that gets queued and then evicted before the link comes
    //     back — the call succeeded at *accepting* the bytes, not at
    //     *delivering* them. A caller that only checks `is_ok()` has no
    //     way to notice frames going missing; that's what `stats_handle()`
    //     is for.
    // ---------------------------------------------------------------------
    let policy = ReconnectPolicy {
        mode: ReconnectMode::Background,
        max_attempts: Some(20),
        backoff: BackoffStrategy::Exponential {
            base: Duration::from_millis(50),
            max: Duration::from_millis(500),
        },
        gap_buffer_capacity: 4,
        overflow_policy: OverflowPolicy::DropOldest,
    };
    let managed = ManagedTransport::new(initial, factory, policy);

    // Grab the stats handle BEFORE moving `managed` into the sender shell.
    // `MuxSender::new` takes `managed` by value — once it's moved, there's
    // no way to get it back short of `MuxSender::into_inner()` (which would
    // also stop the pipeline). `stats_handle()` sidesteps that entirely: it
    // clones a couple of `Arc`s pointing at the same shared gap-buffer/
    // reconnect state that lives inside `managed`, so the handle keeps
    // reading live counters no matter who owns the transport afterward —
    // the same pattern as `Socket::cancel_handle()`.
    let stats = managed.stats_handle();
    let sender = MuxSender::new(managed, MuxerConfig::default())?;

    eprintln!(
        "sender: sending {NUM_FRAMES} frames; peer goes unreachable for ~300ms after 5 messages"
    );
    let mut sent_ok = 0usize;
    let mut sent_err = 0usize;
    for i in 0..NUM_FRAMES {
        // 90 kHz TS clock. 90000 Hz / 100 fps (matching FRAME_INTERVAL's
        // 10ms cadence) = 900 ticks per frame.
        let pts = (i as i64) * 900;
        let nal = synthetic_nal_au(800);
        let klv = synthetic_klv(64, i as i64);
        match sender.send_video(&nal, Pts90khz::new(pts), i == 0) {
            // In `Background` mode, `Ok(())` here means "accepted" — either
            // sent live, or queued in the gap buffer. It does NOT mean the
            // bytes reached the peer; see the policy comment above.
            Ok(()) => sent_ok += 1,
            // An `Err` this loop only sees is the give-up report described
            // above (`max_attempts` exhausted, or a worker panic) — normal
            // transient outages never surface here in `Background` mode,
            // unlike `Blocking` mode where every outage produces visible
            // per-call errors while the caller is stalled waiting.
            Err(e) => {
                eprintln!("sender: send_video {i} -> {e:?}");
                sent_err += 1;
            }
        }
        match sender.send_klv(&klv, Pts90khz::new(pts), 0x00) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("sender: send_klv {i} -> {e:?}");
                sent_err += 1;
            }
        }

        // Stats printout every 10 frames (~100ms of wall time at this
        // cadence) — enough resolution to watch the outage happen without
        // flooding stderr. `sender.is_alive()` passes through to the
        // wrapped `ManagedTransport`, which reports `true` for the whole
        // time a background worker is actively recovering (it only ever
        // reports `false` once there's no live inner transport AND no
        // worker trying to rebuild one) — so don't read `is_alive() ==
        // false` here as "the link is up"; read `is_alive() == true` as
        // "either connected, or recovering, but not abandoned."
        if i % 10 == 0 {
            if let Some(s) = stats.stats() {
                eprintln!(
                    "sender: frame {i} alive={} reconnecting={} gap_len={} \
                     attempts={} successes={} dropped_msgs={} dropped_bytes={}",
                    sender.is_alive(),
                    s.reconnecting,
                    s.gap_len,
                    s.reconnect_attempts,
                    s.reconnect_successes,
                    s.gap_messages_dropped,
                    s.gap_bytes_dropped,
                );
            }
        }

        // 10ms cadence — fast enough that the outage produces meaningfully
        // more sends than `gap_buffer_capacity` can hold (see the
        // NUM_FRAMES/FRAME_INTERVAL comment up top), which is what makes
        // the eviction count in the final stats line below reliably
        // nonzero.
        thread::sleep(FRAME_INTERVAL);
    }
    eprintln!("sender: {sent_ok} OK, {sent_err} errored across the run");
    sender.close();

    if let Some(s) = stats.stats() {
        eprintln!(
            "final stats: attempts={} successes={} gap_len={} dropped_msgs={} dropped_bytes={}",
            s.reconnect_attempts,
            s.reconnect_successes,
            s.gap_len,
            s.gap_messages_dropped,
            s.gap_bytes_dropped,
        );
    }

    let _ = peer_handle.join();
    println!(
        "OK: completed run with a background reconnect (sent_ok={sent_ok}, sent_err={sent_err})"
    );
    Ok(())
}

// Synthetic H.264 access unit — see `managed_reconnect.rs` for the byte
// layout rationale (Annex-B start code + IDR NAL header + filler).
fn synthetic_nal_au(n: usize) -> Vec<u8> {
    let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    buf.extend(std::iter::repeat(0xAA).take(n));
    buf
}

// Synthetic KLV blob — see `managed_reconnect.rs` for the byte layout
// rationale (canonical ST 0601 UL + BER short-form length + filler).
fn synthetic_klv(n: usize, seq: i64) -> Vec<u8> {
    let mut buf = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ];
    buf.push(n as u8);
    buf.extend(std::iter::repeat(seq as u8).take(n));
    buf
}
