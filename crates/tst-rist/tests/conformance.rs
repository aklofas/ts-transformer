//! WP-C1's transport conformance kit over librist loopback (spec §3.5 / §6).
//! Shape = `crates/tst-tcp/tests/conformance.rs` with a librist peer.
//!
//! Three librist facts shape the factories:
//!
//! 1. **Port rotation.** Re-binding a port right after `rist_destroy` races
//!    librist's socket teardown (the "ephemeral-bind + librist-rebind race"
//!    `loopback.rs` documents), so every receiver factory call takes the
//!    NEXT even port from a private range (33050–33076; senders 33080–33098),
//!    disjoint from `loopback.rs` 33010–33026, `cancel.rs` 33040–33048, the
//!    C test's 33100 and the pytest suite 34110–34150. Simple profile needs
//!    EVEN ports (`rist.c:866`).
//! 2. **Handshake warm-up inside `feed`.** A receiver parks at once (each
//!    `recv_bytes` is a 100 ms tick), but DATA flows only after the ~500 ms
//!    Simple-profile handshake and librist may drop the first packets while
//!    it settles (`loopback.rs` accepts 3 of 5). `feed` therefore builds a
//!    sender to the current port, waits 600 ms, then sends the payload 10×
//!    at 50 ms spacing (~1.1 s) — within the kit's `WATCHDOG` (10 s) for the
//!    receive that follows. The kit's feed contract is "delivers SOMETHING
//!    on the next successful recv" (duplicates are fine).
//! 3. **The feed sender must outlive the feed.** A sender destroyed right
//!    after its burst can take the flow down before the receiver's
//!    recovery buffer (200 ms default) releases the blocks, so feeders are
//!    parked in a `Vec` and dropped at the end of the test.
//!
//! Send rows run with `SendPark::NextCall`: `rist_sender_data_write`
//! enqueues and never parks; the `Loop` model would only stuff librist's
//! 524 288-entry queue until the cancel landed.
//!
//! Both sides pass `BrokenSource::NotProducible`, so `not_alive_after_broken`
//! (send + recv) and `peer_eof_is_not_a_cancel` (recv only) print visible skip
//! lines: after `connect_with_config` the only fatal `rist_sender_data_write`
//! code is the zero-length payload, which `send_bytes` refuses before librist
//! sees it (CORR-04), and `rist_receiver_data_read2` only fails on a
//! null/non-receiver ctx, which `close()`'s `alive` check pre-empts. Those
//! latch lines are unit-pinned by `transport::tests` /
//! `recv::tests::close_destroys_ctx_even_when_already_dead` via
//! `force_dead_for_test`.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tst_core::transport::Transport;
use tst_core::transport::conformance::{
    self as kit, BrokenSource, PostCloseKind, RecvOptions, RecvRow, SendOptions, SendPark, SendRow,
};
use tst_rist::{
    RistProfile, RistRecvTransport, RistRecvTransportBuilder, RistTransport, RistTransportBuilder,
};

static NEXT_RECV_PORT: AtomicU16 = AtomicU16::new(33050);
static NEXT_SEND_PORT: AtomicU16 = AtomicU16::new(33080);

/// RIST rides RTP over UDP: the deliverable ceiling is the UDP datagram
/// maximum, which is exactly what `RistRecvTransport::max_payload` returns
/// (`recv.rs`: "65535 … matching the UDP transport's convention"; librist
/// additionally caps blocks at 10 000 B, below this).
const RIST_RECV_CEILING: usize = 65_535;

fn next_port(counter: &AtomicU16, ceiling: u16) -> u16 {
    let p = counter.fetch_add(2, Ordering::SeqCst);
    assert!(
        p < ceiling,
        "port range exhausted — the kit built more transports than budgeted"
    );
    p
}

fn sender_to(port: u16) -> RistTransport {
    RistTransportBuilder::new(&format!("rist://127.0.0.1:{port}"))
        .expect("url")
        .profile(RistProfile::Simple)
        .connect()
        .expect("connect RistTransport")
}

#[test]
fn rist_send_contract_loopback() {
    kit::assert_send_rows(
        // No receiver: a librist sender needs no peer to enqueue.
        || sender_to(next_port(&NEXT_SEND_PORT, 33098)),
        // No fatal write code is reachable after connect (CORR-04) — the
        // kit prints its skip line; the latch is unit-pinned in transport.rs.
        BrokenSource::<RistTransport>::NotProducible,
        SendOptions {
            post_close: PostCloseKind::Closed,
            park: SendPark::NextCall,
        },
        SendRow::ALL,
    );
}

/// Receiver factory + handshake-aware feed sharing one "current port" cell;
/// `feeders` keeps every feed sender alive until the caller drops it.
fn recv_factory_and_feed(
    feeders: Arc<Mutex<Vec<RistTransport>>>,
) -> (impl Fn() -> RistRecvTransport, impl Fn(&[u8])) {
    let current_port: Arc<Mutex<u16>> = Arc::new(Mutex::new(0));
    let port_for_factory = Arc::clone(&current_port);
    let port_for_feed = Arc::clone(&current_port);
    let factory = move || {
        let port = next_port(&NEXT_RECV_PORT, 33078);
        let recv = RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{port}"))
            .expect("url")
            .profile(RistProfile::Simple)
            .listen()
            .expect("listen RistRecvTransport");
        *port_for_factory.lock().unwrap() = port;
        recv
    };
    let feed = move |payload: &[u8]| {
        let port = *port_for_feed.lock().unwrap();
        assert_ne!(port, 0, "feed called before any factory call");
        let mut send = sender_to(port);
        // Handshake warm-up (Simple ≈ 500 ms on Linux loopback), then a
        // burst so at least one copy survives librist's settling loss.
        thread::sleep(Duration::from_millis(600));
        for _ in 0..10 {
            let _ = send.send_bytes(payload);
            thread::sleep(Duration::from_millis(50));
        }
        feeders.lock().unwrap().push(send); // outlives the feed — see the module doc
    };
    (factory, feed)
}

#[test]
fn rist_recv_contract_loopback() {
    let feeders = Arc::new(Mutex::new(Vec::new()));
    let (factory, feed) = recv_factory_and_feed(Arc::clone(&feeders));
    kit::assert_recv_rows(
        factory,
        feed,
        BrokenSource::<RistRecvTransport>::NotProducible,
        RecvOptions::default(),
        RecvRow::ALL,
    );
    drop(feeders); // rist_destroy on every feed sender, after the rows
}

#[test]
fn rist_recv_max_payload_ge_ceiling_loopback() {
    let feeders = Arc::new(Mutex::new(Vec::new()));
    let (factory, _feed) = recv_factory_and_feed(feeders);
    kit::recv_max_payload_ge_ceiling(&factory(), RIST_RECV_CEILING);
}
