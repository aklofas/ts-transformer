//! Cross-thread cancel contract for `UdpTransport` / `UdpRecvTransport`
//! (deep review #4 Arc 2, WP-D). Every test name carries `loopback` so
//! nextest funnels the binary through the serialised `network` group.
//!
//! The contract these pin (spec §3.5, UDP rows):
//! - `cancel()` from any thread makes the *next* `send_bytes` return
//!   `ExplicitClose`, and a `recv_bytes` parked on the 100 ms poll loop
//!   returns `ExplicitClose` at its next tick;
//! - `is_alive()` is `false` after a cancel AND after a `Broken`;
//! - `close()` still means `Closed` — a cancel is the only producer of
//!   `ExplicitClose`.
//!
//! No wall-clock-duration assert anywhere: the cancel tests bound the wait
//! with a watchdog that FAILS (never hangs) and rescue the parked worker
//! before the panic so the process exits cleanly.

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_udp::{UdpRecvTransport, UdpTransport, UdpTransportBuilder};

/// A std socket the sender tests aim at; kept alive so the port stays bound.
fn sink() -> (UdpSocket, u16) {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind sink");
    let port = s.local_addr().unwrap().port();
    (s, port)
}

/// A cancelled sender's NEXT send is `ExplicitClose` (not `Closed`, not a
/// successful datagram), and the transport reports dead. RED on the
/// pre-WP-D tree: `cancel_handle()` returns `None` and the `expect` fires.
#[test]
fn send_after_cancel_loopback_is_explicit_close() {
    let (_sink, port) = sink();
    let mut send = UdpTransport::connect(&format!("udp://127.0.0.1:{port}")).expect("connect");
    let handle = Transport::cancel_handle(&send).expect("UdpTransport must expose a cancel handle");
    assert!(
        !handle.is_cancelled(),
        "fresh handle must not read cancelled"
    );
    send.send_bytes(&[0x47u8; 188]).expect("send before cancel");

    // Cancel from another thread — the whole point of the handle.
    let h2 = handle.clone();
    thread::spawn(move || h2.cancel()).join().unwrap();

    assert!(
        handle.is_cancelled(),
        "is_cancelled must flip after cancel()"
    );
    let r = send.send_bytes(&[0x47u8; 188]);
    assert!(
        matches!(r, Err(TransportError::ExplicitClose)),
        "expected ExplicitClose after cancel, got {r:?}"
    );
    assert!(!send.is_alive(), "a cancelled transport must report dead");
}

/// `close()` is NOT a cancel: post-close sends are `Closed`, and the handle
/// obtained earlier does not read cancelled (close and cancel are distinct
/// signals — the kit's `post_close_is_closed` row).
#[test]
fn send_after_close_loopback_is_closed_not_explicit_close() {
    let (_sink, port) = sink();
    let mut send = UdpTransport::connect(&format!("udp://127.0.0.1:{port}")).expect("connect");
    let handle = send.cancel_handle();
    send.close();
    send.close(); // idempotent
    let r = send.send_bytes(&[0x47u8; 188]);
    assert!(matches!(r, Err(TransportError::Closed)), "got {r:?}");
    assert!(!send.is_alive());
    assert!(!handle.is_cancelled(), "close() must not read as a cancel");
}

/// A fatal send error latches the transport dead (spec §3.5 table: UDP
/// `is_alive()` after `Broken` = false; was: no latch). The deterministic
/// fatal error is `EMSGSIZE`: with `pkt_size` raised above the IPv4 UDP
/// maximum (65 507 B) the `TooLarge` guard lets a 66 000-byte datagram
/// through to `send_to`, which the kernel refuses on every platform
/// (Linux/macOS `EMSGSIZE`, Windows `WSAEMSGSIZE`) — a non-transient
/// `io::ErrorKind` per `classify_send_error`, hence `Broken`. RED on the
/// pre-WP-D tree: `Broken` is returned but `is_alive()` stays `true`.
#[test]
fn send_broken_loopback_latches_dead() {
    let (_sink, port) = sink();
    // NOTE: the knob setters return `&mut Self` while `build` consumes
    // `self`, so the builder cannot be used as one chained expression.
    let mut builder =
        UdpTransportBuilder::from_url(&format!("udp://127.0.0.1:{port}")).expect("url");
    builder.pkt_size(70_000);
    let mut send = builder.build().expect("build");
    let oversize = vec![0x47u8; 66_000];
    let r = send.send_bytes(&oversize);
    assert!(
        matches!(r, Err(TransportError::Broken { .. })),
        "a 66000-byte datagram must be refused by the kernel as Broken, got {r:?}"
    );
    assert!(!send.is_alive(), "Broken must latch the transport dead");
    let next = send.send_bytes(&[0x47u8; 188]);
    assert!(
        matches!(next, Err(TransportError::Closed)),
        "sends after a latched Broken are Closed, got {next:?}"
    );
}

/// The Arc 1 CORR-24 contract is untouched by the latch: a `Backpressure`
/// (deadline / EINTR class) never latches. There is no loopback producer for
/// a UDP send deadline, so this pins the alive-after-TooLarge half instead —
/// an input error that must not latch either.
#[test]
fn send_too_large_loopback_does_not_latch() {
    let (_sink, port) = sink();
    let mut send = UdpTransport::connect(&format!("udp://127.0.0.1:{port}")).expect("connect");
    let r = send.send_bytes(&[0u8; 1317]);
    assert!(
        matches!(
            r,
            Err(TransportError::TooLarge {
                len: 1317,
                max: 1316
            })
        ),
        "got {r:?}"
    );
    assert!(
        send.is_alive(),
        "TooLarge is an input error; the transport stays alive"
    );
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// Outcome of a parked receive, sent back over a channel so the test thread
/// can bound the wait and rescue the worker on failure.
enum Parked {
    Ended(
        Result<usize, TransportError>,
        bool, /* is_alive after */
    ),
}

/// Park `recv` on a worker thread; returns the channel, the port a rescue
/// datagram must be sent to, the join handle, and an `entered` latch the
/// worker sets immediately before it calls `recv_bytes`.
///
/// The latch is why the caller does not guess with a sleep: it proves the
/// worker actually reached the blocking call, so the cancel that follows
/// is genuinely exercising the PARK path rather than the entry check.
fn park_recv(
    mut recv: UdpRecvTransport,
) -> (
    mpsc::Receiver<Parked>,
    u16,
    thread::JoinHandle<()>,
    Arc<AtomicBool>,
) {
    let port = recv.local_addr().port();
    let (tx, rx) = mpsc::channel();
    let entered = Arc::new(AtomicBool::new(false));
    let e = Arc::clone(&entered);
    let worker = thread::spawn(move || {
        let mut buf = vec![0u8; recv.max_payload()];
        e.store(true, Ordering::SeqCst);
        let r = recv.recv_bytes(&mut buf);
        let alive = recv.is_alive();
        let _ = tx.send(Parked::Ended(r, alive));
    });
    (rx, port, worker, entered)
}

/// Spin until `cond` or the bound elapses; `false` means it never became true.
fn wait_until(bound: Duration, cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    cond()
}

/// Send one datagram to `port` so a worker that did NOT observe the cancel
/// returns anyway and the process can exit — called only on the failure path.
fn rescue(port: u16) {
    let s = UdpSocket::bind("127.0.0.1:0").expect("rescue bind");
    let _ = s.send_to(&[0x47u8; 188], ("127.0.0.1", port));
}

/// The headline row: a `recv_bytes` parked on the poll loop, cancelled from
/// another thread, returns `ExplicitClose` at its next ~100 ms tick and the
/// transport reports dead.
///
/// Bound: 2 s = 20 poll ticks. This is NOT an elapsed-time assert on the
/// success path (nothing measures how long the cancel took); it is the
/// failure bound after which the test rescues the worker and FAILS. Spec
/// §11's "cancel returns < 1 s" is a property the 100 ms tick guarantees by
/// construction; the bound is 2× that so a loaded Windows runner (the
/// nextest `network` group's slowest platform) cannot turn one late
/// scheduling quantum into a red. RED on the pre-WP-D tree:
/// `cancel_handle()` is `None` → the `expect` fires.
#[test]
fn recv_cancel_from_other_thread_loopback_returns_explicit_close() {
    let recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let handle =
        RecvTransport::cancel_handle(&recv).expect("UdpRecvTransport must expose a cancel handle");
    let (rx, port, worker, entered) = park_recv(recv);
    if !wait_until(Duration::from_secs(5), || entered.load(Ordering::SeqCst)) {
        rescue(port);
        let _ = worker.join();
        panic!("the recv worker never reached recv_bytes");
    }
    // The worker has entered the call; give it the moment it needs to be
    // inside the kernel wait rather than just before it. Not an assertion —
    // a cancel that lands a hair early is still observed at the entry check
    // (`recv_after_cancel_loopback_is_explicit_close_at_entry` pins that).
    thread::sleep(Duration::from_millis(150));
    handle.cancel();
    let outcome = match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(o) => o,
        Err(_) => {
            rescue(port);
            let _ = worker.join();
            panic!("cancel() did not unblock the parked recv_bytes within 2 s (20 poll ticks)");
        }
    };
    worker.join().unwrap();
    let Parked::Ended(r, alive) = outcome;
    assert!(
        matches!(r, Err(TransportError::ExplicitClose)),
        "expected ExplicitClose from the cancelled park, got {r:?}"
    );
    assert!(!alive, "a cancelled receiver must report dead");
    assert!(handle.is_cancelled());
}

/// Cancel BEFORE the park: the entry check returns `ExplicitClose` without
/// touching the socket (no tick is waited out).
#[test]
fn recv_after_cancel_loopback_is_explicit_close_at_entry() {
    let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let handle = recv.cancel_handle();
    handle.cancel();
    let mut buf = vec![0u8; recv.max_payload()];
    let r = recv.recv_bytes(&mut buf);
    assert!(matches!(r, Err(TransportError::ExplicitClose)), "got {r:?}");
    assert!(!recv.is_alive());
}

/// `close()` stays `Closed` on the receive side too, and the poll loop still
/// observes a close from the owning thread's flag (the pre-Arc-2 contract,
/// kept): pin both so the cancel-first change cannot regress them.
#[test]
fn recv_after_close_loopback_is_closed() {
    let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let handle = recv.cancel_handle();
    recv.close();
    recv.close();
    let mut buf = vec![0u8; recv.max_payload()];
    let r = recv.recv_bytes(&mut buf);
    assert!(matches!(r, Err(TransportError::Closed)), "got {r:?}");
    assert!(!recv.is_alive());
    assert!(!handle.is_cancelled(), "close() must not read as a cancel");
}

/// A cancel that lands AFTER a successful receive does not rewrite that
/// success (spec §5): the datagram is returned; only the NEXT call fails.
#[test]
fn recv_success_then_cancel_loopback_keeps_the_datagram() {
    let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let port = recv.local_addr().port();
    let handle = recv.cancel_handle();
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.send_to(&[0x47u8; 188], ("127.0.0.1", port)).unwrap();
    let mut buf = vec![0u8; recv.max_payload()];
    let n = recv
        .recv_bytes(&mut buf)
        .expect("the queued datagram is delivered");
    assert_eq!(n, 188);
    handle.cancel();
    let r = recv.recv_bytes(&mut buf);
    assert!(matches!(r, Err(TransportError::ExplicitClose)), "got {r:?}");
}

/// X-CORR-07 (the kit row `empty_recv_is_noop`): `recv_bytes(&mut [])` is
/// `Ok(0)` at once and leaves the transport alive and the queue untouched —
/// the datagram sent BEFORE the empty read is still delivered by the next
/// real read. RED on the pre-WP-D tree: the empty read is served by a
/// zero-length `recv` that CONSUMES the queued datagram, so the follow-up
/// read finds nothing (`Ok(None)`).
#[test]
fn recv_empty_buffer_loopback_is_noop() {
    let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let port = recv.local_addr().port();
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.send_to(&[0x47u8; 188], ("127.0.0.1", port)).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let empty = recv.recv_bytes(&mut []);
        let alive = recv.is_alive();
        let mut buf = vec![0u8; recv.max_payload()];
        let next = recv.recv_timeout(&mut buf, Duration::from_secs(2));
        let _ = tx.send((empty, alive, next));
    });
    let (empty, alive, next) = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(v) => v,
        Err(_) => {
            rescue(port);
            let _ = worker.join();
            panic!("recv_bytes(&mut []) parked instead of returning Ok(0) at once");
        }
    };
    worker.join().unwrap();
    assert!(
        matches!(empty, Ok(0)),
        "empty read must be Ok(0), got {empty:?}"
    );
    assert!(alive, "an empty read must not latch the transport");
    assert!(
        matches!(next, Ok(Some(188))),
        "the queued datagram must survive the empty read, got {next:?}"
    );
}
