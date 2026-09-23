//! WP-C1 — the tst-core transport conformance kit over live SRT loopback
//! pairs (spec §3.5). One listener per test; every factory call dials it from
//! a helper thread while this thread accepts, and the accepted socket is kept
//! in `peer` so the connection outlives the row (`drop_peer` is the
//! `break_wire` of the `not_alive_after_broken` and `peer_eof_is_not_a_cancel`
//! rows). 200 ms recv / 5 s send timeouts on both ends; a parked recv
//! surfaces as a `Backpressure` tick every 200 ms, which the kit's park loops
//! retry.
//!
//! `peer_eof_is_not_a_cancel` is the row the WP-C1 SRT latch split exists
//! for. `SrtCancelHandle::is_cancelled()` used to read the "closer has run"
//! sentinel, which `Socket::drop` sets on every transport error path that
//! retires a dead socket — so a peer that merely went away was
//! indistinguishable from a caller cancel, and every clean SRT end-of-stream
//! was reported as a caller close at the C ABI (`TST_E_CLOSED` for
//! `TST_E_END_OF_STREAM`). This row runs it against a real peer close.

#[macro_use]
#[path = "common/mod.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tst_core::transport::conformance::{self as kit, BrokenSource, RecvRow, SendPark, SendRow};
use tst_core::transport::{RecvTransport, TransportError};
use tst_srt::{Listener, ListenerBuilder, Socket, SocketBuilder, SrtTransport};

/// libsrt's live-mode `SRTO_PAYLOADSIZE` maximum — what a foreign peer at the
/// maximum delivers regardless of our local value (`SRT_LIVE_MAX_PAYLOAD` in
/// `src/transport.rs`).
const SRT_LIVE_MAX_PAYLOAD: usize = 1456;

/// Both ends' `SRTO_RCVTIMEO`. Short on purpose: a parked receive surfaces as
/// a `Backpressure` tick the kit's park loops retry, and the warm-up below
/// costs one tick per lost datagram rather than one multi-second stall.
const RECV_TICK: Duration = Duration::from_millis(200);

/// Prove the listener→caller direction carries before handing the transport
/// to a row.
///
/// libsrt drops datagrams sent on a connection whose `accept()` has only just
/// returned — the caller side is still finalising. A row that calls `feed()`
/// and then receives (`empty_recv_is_noop`) would lose its probe and fail on
/// a cold connection. The in-tree convention for this class is
/// `common::settle()`, a 100 ms sleep; looping until a datagram actually
/// lands is the same idea made deterministic — no wall-clock assertion, and a
/// connection that never carries fails loudly here instead of ten seconds
/// later inside a row.
///
/// The caller's receive buffer is drained afterwards so every row starts
/// clean: `not_alive_after_cancel` performs exactly ONE receive and would
/// read a leftover warm-up datagram as success.
fn warm_up(peer: &mut Socket, t: &mut SrtTransport) {
    const WARMUP: [u8; 188] = [0x47; 188];
    let mut buf = vec![0u8; 2048];
    let mut warmed = false;
    for _ in 0..40 {
        peer.send(&WARMUP).expect("warm-up send");
        match RecvTransport::recv_bytes(t, &mut buf) {
            Ok(_) => {
                warmed = true;
                break;
            }
            Err(TransportError::Backpressure { .. }) => continue,
            Err(e) => panic!("warm-up receive failed: {e:?}"),
        }
    }
    assert!(
        warmed,
        "the SRT loopback never delivered a warm-up datagram from the accepted peer"
    );
    // One more tick: drop any straggler that arrived after a timed-out
    // attempt, so the row that follows sees only what it fed.
    loop {
        match RecvTransport::recv_bytes(t, &mut buf) {
            Ok(_) => continue,
            Err(TransportError::Backpressure { .. }) => break,
            Err(e) => panic!("warm-up drain failed: {e:?}"),
        }
    }
}

struct Pair {
    listener: Mutex<Listener>,
    port: u16,
    peer: Mutex<Option<Socket>>,
    /// Pre-obtained at bind, fired by `Drop` — including on an unwind out of
    /// a failed row. libsrt's GC can prune a broken not-yet-accepted
    /// connection from the accept queue, leaving a blocking `accept()` parked
    /// forever (`reference_libsrt_accept_queue_prune_hang`, PR #231); a
    /// listener handle obtained BEFORE the listener moves anywhere is the
    /// sanctioned wake. Our `connect()` accepts against a caller thread that
    /// stays alive, so the hazard should not arise — this guard is what makes
    /// "should not" not matter.
    accept_cancel: tst_core::SrtCancelHandle,
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.accept_cancel.cancel();
    }
}

impl Pair {
    fn bind() -> Self {
        let mut b = ListenerBuilder::new();
        b.recv_timeout(RECV_TICK)
            .send_timeout(Duration::from_secs(5));
        let listener = b.bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local_addr").port();
        let accept_cancel = listener.cancel_handle();
        Self {
            listener: Mutex::new(listener),
            port,
            peer: Mutex::new(None),
            accept_cancel,
        }
    }

    /// The transport under test is the CALLER socket; the accepted socket is
    /// the silent peer. Accept runs on this thread against a caller thread
    /// that stays alive until the handshake completes, so no parked accept is
    /// ever left behind.
    fn connect(&self) -> SrtTransport {
        let port = self.port;
        let caller = thread::spawn(move || {
            let mut b = SocketBuilder::new();
            b.recv_timeout(RECV_TICK)
                .send_timeout(Duration::from_secs(5));
            b.connect(format!("127.0.0.1:{port}"))
                .expect("caller connect")
        });
        let (mut accepted, _peer_addr) = self.listener.lock().unwrap().accept().expect("accept");
        let caller = caller.join().expect("caller thread");
        let mut t = SrtTransport::new(caller);
        warm_up(&mut accepted, &mut t);
        *self.peer.lock().unwrap() = Some(accepted);
        t
    }

    fn feed(&self, bytes: &[u8]) {
        self.peer
            .lock()
            .unwrap()
            .as_mut()
            .expect("a live peer")
            .send(bytes)
            .expect("peer send");
    }

    /// Close the accepted peer — the clean end-of-stream a conformant
    /// receiver must report as a stream end, never as a caller cancel.
    fn drop_peer(&self) {
        self.peer.lock().unwrap().take();
    }
}

#[test]
fn srt_send_contract_all_but_the_cancel_rows() {
    require_loopback!();
    // `Arc` because `BrokenSource::Induce` boxes a `'static` closure: the
    // inducer cannot borrow a local `Pair`.
    let pair = Arc::new(Pair::bind());
    let breaker = Arc::clone(&pair);
    // Induce = drop the accepted peer: its `srt_close` sends a shutdown the
    // caller's next send/recv observes as ConnectionBroken.
    kit::assert_send_rows(
        || pair.connect(),
        BrokenSource::Induce(Box::new(move |_t: &mut SrtTransport| breaker.drop_peer())),
        kit::SendOptions::default(),
        &SendRow::all_except(&[
            SendRow::CancelDuringParkIsExplicitClose,
            SendRow::CancelBeforeOpIsExplicitClose,
        ]),
    );
}

#[test]
fn srt_send_cancel_rows_are_explicit_close() {
    require_loopback!();
    let pair = Pair::bind();
    kit::send_cancel_during_park_is_explicit_close(pair.connect(), SendPark::Loop);
    kit::send_cancel_before_op_is_explicit_close(pair.connect());
}

#[test]
fn srt_recv_contract_all_but_the_cancel_rows() {
    require_loopback!();
    let pair = Arc::new(Pair::bind());
    let breaker = Arc::clone(&pair);
    // `peer_eof_is_not_a_cancel` runs here, against a real `srt_close` from
    // the accepted peer — the WP-C1 latch split's reason for existing.
    kit::assert_recv_rows(
        || pair.connect(),
        |b| pair.feed(b),
        BrokenSource::Induce(Box::new(move |_r: &mut SrtTransport| breaker.drop_peer())),
        kit::RecvOptions::default(),
        &RecvRow::all_except(&[
            RecvRow::CancelDuringParkIsExplicitClose,
            RecvRow::CancelBeforeOpIsExplicitClose,
        ]),
    );
}

#[test]
fn srt_recv_cancel_rows_are_explicit_close() {
    require_loopback!();
    let pair = Pair::bind();
    kit::recv_cancel_during_park_is_explicit_close(pair.connect());
    kit::recv_cancel_before_op_is_explicit_close(pair.connect());
}

#[test]
fn srt_recv_max_payload_ge_ceiling() {
    require_loopback!();
    let pair = Pair::bind();
    kit::recv_max_payload_ge_ceiling(&pair.connect(), SRT_LIVE_MAX_PAYLOAD);
}
