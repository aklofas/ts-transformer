//! Live-socket validation that `MuxSender::close()` (and the underlying
//! `ResourceCancel`) wakes a peer thread parked inside libsrt's
//! `srt_sendmsg2`.
//!
//! We engineer a deterministic park by:
//! 1. Listening on 127.0.0.1:0 with a tiny SRTO_RCVBUF.
//! 2. Connecting a sender with SRTO_SNDBUF tiny and the libsrt-default
//!    blocking-forever SRTO_SNDTIMEO (-1).
//! 3. NOT calling recv on the listener side — bytes accumulate in the
//!    receive buffer and propagate back-pressure to the sender.
//! 4. Pumping payloads from the sender thread until libsrt's send call
//!    blocks (typically within < 100 packets), and PROVING the park (the
//!    shared `ParkLatch`) before the cancel — a send loop that never parked
//!    would pass the same assertions with the cancel removed.
//! 5. Calling `s.close()` from the main thread; the parked thread must
//!    return within a generous timeout.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::{MuxSender, MuxSenderError, MuxSenderErrorSource, TransportError};
use tst_srt::SrtTransport;
use tst_srt::{SrtListenerBuilder, SrtSocketBuilder};
use tst_test_helpers::timing::scaled;

#[test]
fn close_unblocks_libsrt_parked_send() {
    require_loopback!();
    // Bind a listener with very small recv buffer.
    let mut builder = SrtListenerBuilder::new();
    builder
        .recv_buf_bytes(8 * 1500) // tiny — back-pressure kicks in fast
        .latency(Duration::from_millis(120));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    // Accept thread returns the socket immediately; main thread holds it
    // alive (without recv) so the receive buffer fills and back-pressures
    // the sender.
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    // Connect the sender side. This drives the SRT handshake, which
    // unblocks `accept()` on the listener side.
    let socket = SrtSocketBuilder::new()
        .send_buf_bytes(8 * 1500) // tiny — sender's outgoing queue saturates fast
        // (no send_timeout — defaults to -1 = block forever)
        .latency(Duration::from_millis(120))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    // Drain the accepted socket out of the accept thread and hold it on
    // the main thread for the duration of the test. Connection stays
    // open from libsrt's view (the Drop at end-of-scope closes it).
    let _peer_socket = accept.join();

    let transport = SrtTransport::new(socket);
    // Obtained before the sender owns the transport: if `close()` blocks
    // (a close that skips the cancel waits on the lock the parked send
    // holds), the watchdog below fires this to release the send.
    let watchdog_cancel =
        Transport::cancel_handle(&transport).expect("SrtTransport has a cancel handle");
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let s = Arc::new(MuxSender::new(transport, cfg).unwrap());
    let s_send = s.clone();

    // MuxSender thread: pump 64-byte NAL payloads until a call parks. 1000
    // NALs is ~64 KB, far more than the 8-packet SNDBUF can absorb. Every
    // completed call bumps the latch, so a loop still making progress is
    // never mistaken for a park.
    let latch = crate::common::ParkLatch::default();
    let worker = latch.clone();
    let send_thread = latch.park(move || -> Result<u32, MuxSenderError> {
        // 4-byte Annex B start code (0x00000001) + 60 bytes payload.
        // The muxer rejects NALs without a start code as InvalidNal.
        let mut nal = vec![0u8; 64];
        nal[3] = 0x01;
        let mut count = 0u32;
        for pts in 0..1000 {
            s_send.send_video(&nal, Pts90khz::new(pts * 90), false)?;
            worker.bump();
            count += 1;
        }
        Ok(count)
    });

    // The cancel below must land on a send that is parked inside libsrt.
    latch.assert_parked();

    // Cancel via close from another thread. Must be near-instant — no
    // mutex contention with the parked thread. Run on a helper thread so a
    // close that blocks fails here, by the watchdog, instead of hanging the
    // test binary.
    let close_start = Instant::now();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let s_close = s.clone();
    let close_thread = std::thread::spawn(move || {
        s_close.close();
        let _ = closed_tx.send(());
    });
    if closed_rx
        .recv_timeout(scaled(Duration::from_secs(5)))
        .is_err()
    {
        watchdog_cancel.cancel();
        panic!("close() blocked on a parked send");
    }
    let close_elapsed = close_start.elapsed();
    close_thread.join().expect("close thread panic");
    assert!(
        close_elapsed < scaled(Duration::from_millis(500)),
        "close() took {close_elapsed:?} — should be near-instant via cancel"
    );

    // Parked sender should return promptly with `ExplicitClose`: the
    // close fires the cancel handle first, libsrt's srt_sendmsg2 returns
    // SRT_ECONNLOST, and `SrtTransport` reads the fired latch and
    // reports the cancel rather than the wire error it provoked.
    let join_start = Instant::now();
    let result = send_thread.join().expect("send thread panic");
    let join_elapsed = join_start.elapsed();
    assert!(
        join_elapsed < scaled(Duration::from_secs(2)),
        "send thread didn't unpark within 2s of cancel ({join_elapsed:?})"
    );
    match result {
        // Two legal outcomes: the parked send woke on the cancel
        // (`ExplicitClose`, the usual one), or the sender was between
        // calls when `close()` won the send lock and closed the transport
        // (`Closed`). Both prove cancel works; we fail on stuck, on a loop
        // that finished (it was never parked), and on `Broken`, which this
        // path never reports.
        Ok(_) => panic!("the send loop finished before the cancel; nothing was parked"),
        Err(ref err)
            if matches!(
                err.source,
                MuxSenderErrorSource::Transport(
                    TransportError::ExplicitClose | TransportError::Closed
                )
            ) => {}
        Err(other) => panic!("unexpected sender error after cancel: {other:?}"),
    }
}

/// After a cancel, EVERY later op reports `ExplicitClose` —
/// not `ExplicitClose` once and `Closed` afterwards; after the transport's
/// OWN `close()` the answer is `Closed` even though `Socket::close` fires
/// the same libsrt cancel latch.
#[test]
fn cancelled_srt_transport_reports_explicit_close_on_every_later_op_and_closed_after_own_close() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    // The accepted socket is returned so the main thread can hold the
    // connection open for the whole test — a peer that went away would
    // give `Broken` for a reason that has nothing to do with the cancel.
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SrtSocketBuilder::new()
        .recv_timeout(Duration::from_millis(200))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    let _peer = accept.join();

    let mut t = SrtTransport::new(socket);
    let h = Transport::cancel_handle(&t).expect("a live transport has a handle");
    h.cancel();

    let mut buf = vec![0u8; RecvTransport::max_payload(&t)];
    for i in 0..3 {
        let r = RecvTransport::recv_bytes(&mut t, &mut buf);
        assert!(
            matches!(r, Err(TransportError::ExplicitClose)),
            "recv #{i} after cancel: {r:?}"
        );
    }
    let r = Transport::send_bytes(&mut t, &[0x47u8; 188]);
    assert!(
        matches!(r, Err(TransportError::ExplicitClose)),
        "send after cancel: {r:?}"
    );
    assert!(
        !Transport::is_alive(&t),
        "a cancelled transport is not alive"
    );

    Transport::close(&mut t);
    let r = RecvTransport::recv_bytes(&mut t, &mut buf);
    assert!(
        matches!(r, Err(TransportError::Closed)),
        "own close() wins over the latch: {r:?}"
    );
    let r = Transport::send_bytes(&mut t, &[0x47u8; 188]);
    assert!(
        matches!(r, Err(TransportError::Closed)),
        "own close() wins over the latch on send: {r:?}"
    );
}
