//! WP-C1: a peer disconnect must NOT latch `SrtCancelHandle::is_cancelled()`.
//!
//! `is_cancelled()` used to read the `CANCELLED` sentinel in the handle's
//! atomic — i.e. "the closer has run". But the closer also runs on the
//! OWNER's teardown: `Socket::drop` fires the handle, and `SrtTransport`
//! drops its `Option<Socket>` on every peer-break path (`transport.rs`
//! recv `:280/:294/:313`, send `:193/:201/:225`). So a peer that simply
//! went away latched the same bit a caller's `cancel()` does.
//!
//! That is load-bearing, not cosmetic: once `Owned::is_cancelled` ORs the
//! transport's latch in (WP-C1), `record_recv_error`'s
//! `broken_is_eos && !cancelled` guard stops firing and every clean SRT
//! end-of-stream is reported as a caller close — `TST_E_CLOSED` (−7)
//! instead of `TST_E_END_OF_STREAM` (−12) at the C ABI, and the same
//! one-kind shift in Python and the JVM. Four `tst-c` receiving tests
//! caught it.
//!
//! The fix splits the two: `SrtCancelHandle::cancel()` (the caller path)
//! latches; `close_without_cancel()` (owner teardown: both `Drop` impls)
//! closes and wakes without latching.

use std::time::Duration;
use tst_core::transport::{RecvTransport, TransportError};
use tst_srt::{SocketBuilder, SrtTransport};

/// Peer closes cleanly while we hold a receiver: the receive ends (EOS or
/// a Broken-class error, both acceptable at this layer), the transport is
/// no longer alive — and `is_cancelled()` stays FALSE, because nobody
/// cancelled anything.
#[test]
fn peer_close_ends_the_stream_without_latching_is_cancelled() {
    require_loopback!();

    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    // The peer accepts, hands the socket back, and we drop it below to
    // produce the clean disconnect.
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .recv_timeout(Duration::from_millis(200))
        .latency(Duration::from_millis(120))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    // Obtain-before-move: the typed handle comes off the Socket before it
    // is consumed by the transport.
    let handle = socket.cancel_handle();
    let mut rx = SrtTransport::new(socket);
    assert!(
        !handle.is_cancelled(),
        "a freshly connected receiver's handle must not read cancelled"
    );

    // Clean peer-side close.
    let peer = accept.join();
    peer.close().expect("peer close");

    // Drive the receiver until it observes the end. `Backpressure` is the
    // recv-timeout tick — keep going. Bounded by attempts, not wall clock:
    // a regression fails the assertion below instead of hanging.
    let mut buf = vec![0u8; 1500];
    let mut outcome = None;
    for _ in 0..100 {
        match rx.recv_bytes(&mut buf) {
            Ok(_) => continue,
            Err(TransportError::Backpressure { .. }) => continue,
            other => {
                outcome = Some(other);
                break;
            }
        }
    }
    let outcome = outcome.expect("the receiver never observed the peer's close");

    assert!(
        !RecvTransport::is_alive(&rx),
        "the transport must be dead after the peer closed (outcome {outcome:?})"
    );
    assert!(
        !handle.is_cancelled(),
        "a PEER close latched is_cancelled() (outcome {outcome:?}) — that bit is the \
         CALLER's cancel latch; latching it here relabels every clean SRT end-of-stream \
         as a caller close in the C ABI and both other bindings"
    );
    // A handle obtained after the break must agree.
    if let Some(again) = RecvTransport::cancel_handle(&rx) {
        assert!(
            !again.is_cancelled(),
            "a handle obtained after a peer close reads cancelled"
        );
    }
}

/// The other half of the contract: a real caller cancel DOES latch, so the
/// split above did not simply break `is_cancelled()`.
#[test]
fn caller_cancel_still_latches_is_cancelled() {
    require_loopback!();

    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .recv_timeout(Duration::from_millis(200))
        .latency(Duration::from_millis(120))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    let handle = socket.cancel_handle();
    let rx = SrtTransport::new(socket);
    let _peer = accept.join();
    let _ = &rx;

    assert!(!handle.is_cancelled());
    handle.cancel();
    assert!(
        handle.is_cancelled(),
        "a caller cancel must latch is_cancelled()"
    );
}
