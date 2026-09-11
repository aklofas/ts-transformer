//! Pipeline-shell integration: MuxSender<TcpTransport> → TCP → DemuxReceiver<TcpTransport>.
//!
//! Mirrors crates/tst-udp/tests/pipeline_round_trip.rs but uses TCP. TCP is
//! bytestream-oriented so we don't need datagram-alignment tricks — the
//! sync-state machine just needs ≥ 941 bytes of aligned 0x47-framed data.
//!
//! # Sync-lock requirement
//!
//! The `Receiver` sync state machine inside `DemuxReceiver` requires at least
//! 5 consecutive 188-byte-aligned sync bytes (0x47) before it transitions from
//! VERIFY to LOCKED and begins emitting events — roughly 941 bytes.
//!
//! Strategy (mirrors `crates/tst-pipeline/tests/pipeline_receiver.rs`):
//! use `psi_interval_ms(10)` so PAT + PMT re-emit on every push call that
//! crosses the 900-tick boundary, then send several frames at 9001-tick
//! spacing.  After a few sends the sync state machine has seen ≥ 941 bytes of
//! aligned 0x47-framed data and will lock, allowing the demuxer to emit a
//! ProgramMap event.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxEvent;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::{
    BackoffStrategy, BrokenCause, DemuxReceiver, ManagedRecvTransport, MuxSender, ReconnectPolicy,
    RecvTransport, ShellErrorKind, TransportError,
};
use tst_tcp::{TcpListener, TcpTransport};

/// Minimal Annex-B H.264 AUD + IDR — just enough for the muxer to accept and
/// frame into a PES.  Semantic validity is not required; the muxer only checks
/// that the first four bytes are the Annex-B start code.
fn synthetic_h264_au() -> Vec<u8> {
    // AUD NAL (nal_type=9, primary_pic_type=0x10) followed by IDR NAL header.
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    v.extend([0x00, 0x00, 0x00, 0x01, 0x65]);
    v.extend(std::iter::repeat(0xab).take(200));
    v
}

#[test]
fn mux_via_tcp_demux_round_trip_recovers_program_map() {
    // -----------------------------------------------------------------------
    // 1. Bind the listener first so the OS assigns a port before we connect.
    // -----------------------------------------------------------------------
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let port = listener.local_addr().unwrap().port();

    // -----------------------------------------------------------------------
    // 2. Build the muxer config.
    //
    //    psi_interval_ms(10) causes PAT + PMT to re-emit on every push call
    //    that crosses the 900-tick boundary, giving the sync-state machine
    //    enough packets to lock in a small number of sends.
    // -----------------------------------------------------------------------
    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().expect("MuxerConfig::build")
    };

    // -----------------------------------------------------------------------
    // 3. Spawn the receiver loop on a dedicated thread.
    //
    //    The thread blocks in accept_blocking() until the caller connects,
    //    then iterates DemuxReceiver events looking for a ProgramMap.
    // -----------------------------------------------------------------------
    let (found_tx, found_rx) = std::sync::mpsc::channel::<bool>();

    let _recv_thread = thread::spawn(move || {
        let recv_transport = listener.accept_blocking().expect("accept");
        let mut receiver = DemuxReceiver::new(recv_transport);
        // DemuxReceiver implements Iterator<Item = Result<DemuxEvent, _>>.
        for item in &mut receiver {
            match item {
                Ok(DemuxEvent::ProgramMap(_)) => {
                    let _ = found_tx.send(true);
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = found_tx.send(false);
                    return;
                }
            }
        }
        // Transport closed before we saw a ProgramMap.
        let _ = found_tx.send(false);
    });

    // Small pause so the receiver thread is blocked in accept_blocking()
    // before we attempt the outbound connect — avoids the race where the
    // connect arrives before the listener has called accept().
    thread::sleep(Duration::from_millis(50));

    // -----------------------------------------------------------------------
    // 4. Connect the sender and push several frames.
    //
    //    Spacing each frame 9001 ticks apart (≈ 10 ms at 90 kHz) crosses the
    //    10-ms PSI re-emission threshold on every push, so PAT + PMT appear
    //    in every outbound batch.  After 4–5 sends the sync state machine has
    //    seen ≥ 941 bytes of aligned 0x47-framed data and will lock, allowing
    //    the demuxer to emit a ProgramMap event.
    // -----------------------------------------------------------------------
    let send_transport =
        TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).expect("connect");
    let sender = MuxSender::new(send_transport, mux_cfg).expect("MuxSender::new");

    let au = synthetic_h264_au();
    for i in 0i64..5 {
        let pts = Pts90khz::new(i * 9001);
        // The receiver thread returns the instant it sees a ProgramMap, which
        // drops its socket. On fast runners that can happen before this loop
        // finishes, so a later send hits a TCP RST ("connection reset by
        // peer"). That's benign here — the receiver already has what it needs.
        // Stop sending on the first broken-transport error; the real assertion
        // is the channel result below, which reports `false` if the connection
        // broke *before* a ProgramMap was recovered.
        // Non-transport error kinds still panic so a real mux/config
        // regression fails loudly (mirrors the tst-udp sibling test).
        match sender.send_video(&au, pts, true) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind,
                    ShellErrorKind::TransportBroken | ShellErrorKind::Closed
                ) =>
            {
                break;
            }
            Err(e) => panic!("send_video failed for a non-transport reason: {e:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // 5. Verify: the receiver emits a ProgramMap within 3 seconds.
    // -----------------------------------------------------------------------
    let ok = found_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("receiver thread did not report within 3 s");

    assert!(ok, "DemuxReceiver did not emit a ProgramMap event");
}

/// E4: a `ManagedRecvTransport<TcpTransport>` parked in `recv_bytes` against
/// a real (silent) TCP peer must be cancellable from another thread.
///
/// Before E1 wired `TcpTransport::cancel_handle` through the trait-level
/// `Transport`/`RecvTransport` overrides, `ManagedRecvTransport` — which can
/// only reach the inner through `dyn RecvTransport` — got `None` back from
/// `inner.cancel_handle()` at construction and never installed a wake handle
/// in its `CancelSlot`. A `cancel()` afterwards latched the managed
/// wrapper's own flag but had nothing to fire against the parked inner read,
/// so the recv thread would never unblock.
///
/// The managed wrapper maps *every* caller-initiated cancel/close to
/// `TransportError::ExplicitClose` — regardless of what the inner transport
/// itself reports (a bare `TcpTransport::recv_bytes` returns `Closed` on
/// cancel; see `loopback.rs`'s `cancel_handle_unblocks_parked_recv`). See the
/// entry-gate + post-inner-call handling in
/// `tst_pipeline::managed_receive::ManagedRecvTransport::recv_bytes`.
#[test]
fn managed_tcp_recv_cancel_unblocks_parked_read() {
    // Silent peer: connect first (the kernel completes the handshake into
    // the listen backlog without anyone calling accept), then accept
    // synchronously and hold the peer socket in a local for the rest of the
    // test. No background thread and no timed sleep to keep it alive -- the
    // connection simply stays open, reading and writing nothing, so the
    // parked recv has no data/EOF/RST to react to and only an explicit
    // cancel can unblock it.
    let peer_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = peer_listener.local_addr().unwrap().port();

    let inner = TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).expect("connect");
    let (_peer_sock, _) = peer_listener.accept().expect("accept");

    // The factory is never actually invoked in this test: cancelling a
    // parked receive is caller-initiated and short-circuits to
    // ExplicitClose before any reconnect attempt is made. max_attempts: 0
    // plus a zero backoff keep the policy inert if that assumption is ever
    // wrong, rather than hanging the test on a real reconnect loop.
    let factory: Box<dyn FnMut() -> Result<TcpTransport, TransportError> + Send> = Box::new(|| {
        Err(TransportError::Broken {
            msg: "factory should not run in this test".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    });
    let policy = ReconnectPolicy {
        max_attempts: Some(0),
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    };

    let mut managed = ManagedRecvTransport::new(inner, factory, policy);

    // Obtain the cancel handle BEFORE moving `managed` into the receiving
    // thread -- this mirrors real usage, where a caller elsewhere holds the
    // handle while a separate loop owns the transport.
    let handle = managed
        .cancel_handle()
        .expect("ManagedRecvTransport::cancel_handle is unconditionally Some");

    let (tx, rx) = mpsc::channel::<Result<usize, TransportError>>();
    let recv_thread = thread::spawn(move || {
        let mut buf = vec![0u8; 1024];
        let result = managed.recv_bytes(&mut buf);
        let _ = tx.send(result);
    });

    // Give the recv thread time to park inside the inner TCP read.
    thread::sleep(Duration::from_millis(100));

    handle.cancel();

    let result = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cancel did not unblock the parked managed receive within 1 s");
    recv_thread.join().unwrap();

    assert_eq!(
        result,
        Err(TransportError::ExplicitClose),
        "managed cancel of a parked TCP read must surface ExplicitClose"
    );
}
