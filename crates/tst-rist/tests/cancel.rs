//! Cross-thread cancel contract for `RistTransport` / `RistRecvTransport`
//! (deep review #4 Arc 2, WP-D). Same shape as `crates/tst-udp/tests/cancel.rs`.
//!
//! Ports are hardcoded EVEN values (Simple profile = RTP on `port` + RTCP on
//! `port + 1`, `rist.c:866`), distinct from `loopback.rs` (33010–33026) and
//! from the pytest suite (34110–34150). A sender needs no listening peer
//! (`rist_sender_data_write` enqueues; the burst test in `loopback.rs`
//! relies on the same fact), so the send-side tests open no receiver.
//!
//! librist's receiver `recv_bytes` is already a 100 ms poll: each call is
//! ONE tick that returns `Backpressure` when nothing arrived. The park in
//! `recv_cancel_…` is therefore a loop over `recv_bytes` that only treats a
//! non-`Backpressure` result as terminal, bounded by its own 10 s deadline
//! so a regression fails instead of hanging the binary.

use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_rist::{RistProfile, RistRecvTransportBuilder, RistTransportBuilder};

static SERIAL: Mutex<()> = Mutex::new(());

const PORT_RECV_CANCEL: u16 = 33040;
const PORT_RECV_CLOSE: u16 = 33042;
const PORT_SEND_CANCEL: u16 = 33044;
const PORT_SEND_CLOSE: u16 = 33046;
const PORT_RECV_EMPTY: u16 = 33048;

/// Build a Simple-profile receiver on `port`, or return `None` when librist
/// cannot bind here (the same skip the sibling suites take).
fn listen(port: u16) -> Option<tst_rist::RistRecvTransport> {
    RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{port}"))
        .ok()?
        .profile(RistProfile::Simple)
        .listen()
        .ok()
}

fn connect(port: u16) -> Option<tst_rist::RistTransport> {
    RistTransportBuilder::new(&format!("rist://127.0.0.1:{port}"))
        .ok()?
        .profile(RistProfile::Simple)
        .connect()
        .ok()
}

/// A cancelled sender's NEXT send is `ExplicitClose`; `is_alive()` reads
/// false. RED on the pre-WP-D tree: `cancel_handle()` is `None`.
#[test]
fn send_after_cancel_loopback_is_explicit_close() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut send) = connect(PORT_SEND_CANCEL) else {
        eprintln!("skip: librist sender unavailable");
        return;
    };
    let handle =
        Transport::cancel_handle(&send).expect("RistTransport must expose a cancel handle");
    assert!(!handle.is_cancelled());
    send.send_bytes(&[0x47u8; 188])
        .expect("send before cancel (no peer needed)");
    let h2 = handle.clone();
    thread::spawn(move || h2.cancel()).join().unwrap();
    assert!(handle.is_cancelled());
    let r = send.send_bytes(&[0x47u8; 188]);
    assert!(matches!(r, Err(TransportError::ExplicitClose)), "got {r:?}");
    assert!(!send.is_alive(), "a cancelled transport must report dead");
}

/// `close()` stays `Closed`, twice is fine, and does not read as a cancel.
#[test]
fn send_after_close_loopback_is_closed() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut send) = connect(PORT_SEND_CLOSE) else {
        eprintln!("skip: librist sender unavailable");
        return;
    };
    let handle = send.cancel_handle();
    send.close();
    send.close();
    let r = send.send_bytes(&[0x47u8; 188]);
    assert!(matches!(r, Err(TransportError::Closed)), "got {r:?}");
    assert!(!send.is_alive());
    assert!(!handle.is_cancelled());
}

enum Parked {
    Ended(Result<usize, TransportError>, bool),
    /// The worker's own 10 s deadline passed with only `Backpressure` ticks —
    /// the cancel was never observed.
    NeverEnded,
}

/// The headline row for RIST: a receiver parked on librist's 100 ms poll,
/// cancelled from another thread, ends with `ExplicitClose` at its next tick.
/// Bound 2 s (20 ticks) on the test thread — a failure bound, not an
/// elapsed-time assert (see the UDP twin for the reasoning); the worker's
/// own 10 s deadline guarantees the binary exits even if the cancel is
/// never observed. RED on the pre-WP-D tree: `cancel_handle()` is `None`.
#[test]
fn recv_cancel_from_other_thread_loopback_returns_explicit_close() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut recv) = listen(PORT_RECV_CANCEL) else {
        eprintln!("skip: librist receiver unavailable on {PORT_RECV_CANCEL}");
        return;
    };
    let handle =
        RecvTransport::cancel_handle(&recv).expect("RistRecvTransport must expose a cancel handle");
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut buf = vec![0u8; recv.max_payload()];
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match recv.recv_bytes(&mut buf) {
                Err(TransportError::Backpressure { .. }) if Instant::now() < deadline => continue,
                Err(TransportError::Backpressure { .. }) => {
                    let _ = tx.send(Parked::NeverEnded);
                    return;
                }
                r => {
                    let alive = recv.is_alive();
                    let _ = tx.send(Parked::Ended(r, alive));
                    return;
                }
            }
        }
    });
    thread::sleep(Duration::from_millis(300));
    handle.cancel();
    let outcome = match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(o) => o,
        Err(_) => {
            // Nothing can un-park a librist receiver from outside without a
            // handshake; the worker's own deadline ends it. Wait for that.
            let late = rx.recv_timeout(Duration::from_secs(10)).ok();
            let _ = worker.join();
            panic!(
                "cancel() did not end the parked recv within 2 s (20 ticks); worker outcome: {}",
                match late {
                    Some(Parked::NeverEnded) => "never observed the cancel",
                    Some(Parked::Ended(..)) => "ended late",
                    None => "no report",
                }
            );
        }
    };
    worker.join().unwrap();
    match outcome {
        Parked::Ended(r, alive) => {
            assert!(matches!(r, Err(TransportError::ExplicitClose)), "got {r:?}");
            assert!(!alive, "a cancelled receiver must report dead");
        }
        Parked::NeverEnded => panic!("worker hit its 10 s deadline without observing the cancel"),
    }
    assert!(handle.is_cancelled());
}

/// `close()` from the owning thread stays `Closed` on the receive side and
/// destroys the librist context (the pre-Arc-2 contract, unchanged).
#[test]
fn recv_after_close_loopback_is_closed() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut recv) = listen(PORT_RECV_CLOSE) else {
        eprintln!("skip: librist receiver unavailable on {PORT_RECV_CLOSE}");
        return;
    };
    let handle = recv.cancel_handle();
    recv.close();
    recv.close();
    let mut buf = vec![0u8; recv.max_payload()];
    let r = recv.recv_bytes(&mut buf);
    assert!(matches!(r, Err(TransportError::Closed)), "got {r:?}");
    assert!(!recv.is_alive());
    assert!(!handle.is_cancelled());
}

/// X-CORR-07 (the kit row `empty_recv_is_noop`): `recv_bytes(&mut [])` is
/// `Ok(0)` and the transport stays alive. (That no librist tick is spent
/// follows from the guard's placement but is not asserted here; the kit's
/// `empty_recv_is_noop` row is the behavioural pin.)
/// RED on the pre-WP-D tree: the call waits out a 100 ms
/// `rist_receiver_data_read2` tick and returns `Backpressure`.
#[test]
fn recv_empty_buffer_loopback_is_noop() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut recv) = listen(PORT_RECV_EMPTY) else {
        eprintln!("skip: librist receiver unavailable on {PORT_RECV_EMPTY}");
        return;
    };
    let r = recv.recv_bytes(&mut []);
    assert!(
        matches!(r, Ok(0)),
        "empty read must be Ok(0) at once, got {r:?}"
    );
    assert!(
        recv.is_alive(),
        "an empty read must not latch the transport"
    );
}
