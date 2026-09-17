//! Pins the two guarantees `crate::common::AcceptHandle` makes about a peer
//! thread whose `accept()` never completes: `join()` fails loudly within
//! its deadline instead of parking forever, and a test that unwinds
//! before `join()` still wakes and reaps the peer (drop guard).
//!
//! The class being defended against: libsrt's GC pass
//! (`CUDTUnited::checkBrokenSockets`) prunes a connection that breaks while
//! still queued on its listener's accept queue. A test that closes its
//! caller socket right after `connect` returns can therefore leave the
//! peer's plain blocking `accept()` with nothing to dequeue — forever,
//! because nothing else ever connects and the listener is owned by the
//! parked thread. It bit tst-c's managed demux receiver test twice in CI
//! (PR #231). No caller ever connects here, which is the same end state
//! without the race.

use std::net::UdpSocket;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

/// A parked `accept()` with no peer must surface as a loud, class-naming
/// failure well inside nextest's per-test kill, never as a hang.
#[test]
#[should_panic(expected = "accept-queue prune")]
fn join_fails_loudly_when_accept_never_completes() {
    if let Err(why) = crate::common::loopback_probe() {
        // `should_panic` cannot skip: satisfy the expectation instead.
        panic!("SKIP (loopback unavailable: {why}) — accept-queue prune");
    }
    let lb = crate::common::Loopback::bind();
    let accept = lb.spawn_accept(|_sock| ());
    accept.wait_ready();
    // Nothing connects. The bounded join fires the listener's cancel
    // handle at its deadline, reaps the woken thread, and panics. A short
    // deadline here only — the default `join` waits `ACCEPT_DEADLINE`.
    accept.join_within(Duration::from_secs(1));
}

/// Unwinding past the handle (an `expect` failing before `join`) must
/// still wake the parked peer and release the listener's port; a leaked
/// thread parked in `srt_accept` would hold the port until process exit.
#[test]
fn drop_guard_wakes_parked_accept_on_unwind() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let unwound = catch_unwind(AssertUnwindSafe(move || {
        let accept = lb.spawn_accept(|_sock| ());
        accept.wait_ready();
        panic!("deliberate panic before join");
    }));
    let payload = unwound.expect_err("the closure panics by construction");
    assert_eq!(
        payload.downcast_ref::<&str>().copied(),
        Some("deliberate panic before join")
    );

    // libsrt releases the listener's UDP port from its GC thread (~1 s
    // cadence) once the socket is closed, so poll. A plain `UdpSocket`
    // bind cannot share a port with a live SRT listener (SO_REUSEADDR is
    // required on BOTH sockets for that), so success here proves the
    // guard closed the listener rather than leaking a parked thread.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UdpSocket::bind(("127.0.0.1", port)) {
            Ok(_) => break,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!(
                "port {port} still held 10 s after the unwind — the peer \
                 thread is leaked in accept(): {e}"
            ),
        }
    }
}
