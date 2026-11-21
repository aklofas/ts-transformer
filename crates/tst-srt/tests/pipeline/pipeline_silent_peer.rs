//! Silent-peer detection over real SRT sockets: the transport's reading of
//! the peer and its non-lingering close.
//!
//! A peer is made silent with the relay ([`crate::relay::Relay`]): it keeps
//! the connection open on both ends but forwards nothing, so the sender's
//! sends keep succeeding while nothing comes back.

use std::net::SocketAddr;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tst_core::transport::{Transport, TransportError};
use tst_srt::{SrtSocket, SrtSocketConfig, SrtTransport};
use tst_test_helpers::timing::scaled;

use crate::common::{AcceptHandle, Loopback};
use crate::relay::Relay;

/// The payload of every message these tests send.
const MSG: [u8; 1316] = [0x47; 1316];

/// A listener whose accepted peer is held open, unread, until the returned
/// sender is dropped or sent on (or the hold runs out).
fn held_peer() -> (u16, AcceptHandle<()>, mpsc::Sender<()>) {
    let lb = Loopback::bind();
    let port = lb.port;
    let (release, released) = mpsc::channel::<()>();
    let accept = lb.spawn_accept(move |peer| {
        let _ = released.recv_timeout(scaled(Duration::from_secs(15)));
        drop(peer);
    });
    accept.wait_ready();
    (port, accept, release)
}

/// An SRT sender with the sender preset (5 s linger), dialled to `addr`.
fn sender_to(addr: SocketAddr) -> SrtTransport {
    let socket =
        SrtSocket::connect_with(&SrtSocketConfig::sender_defaults(), addr).expect("connect");
    SrtTransport::new(socket)
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn peer_activity_moves_once_the_peer_acknowledges() {
    require_loopback!();
    let (port, accept, release) = held_peer();
    let mut t = sender_to(loopback(port));

    let before = t.peer_activity();
    assert!(before.is_some(), "a connected SRT transport has a reading");
    t.send_bytes(&MSG).expect("send");
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    while t.peer_activity() == before {
        assert!(
            Instant::now() < deadline,
            "the reading never moved after a send to a live peer"
        );
        thread::sleep(Duration::from_millis(5));
    }

    t.close();
    drop(release);
    accept.join();
}

#[test]
fn peer_activity_is_none_once_closed() {
    require_loopback!();
    let (port, accept, release) = held_peer();
    let mut t = sender_to(loopback(port));
    t.close();
    assert_eq!(t.peer_activity(), None);
    drop(release);
    accept.join();
}

#[test]
fn abandon_does_not_wait_for_unacknowledged_data() {
    require_loopback!();
    let (port, accept, release) = held_peer();
    let relay = Relay::start(loopback(port));
    let mut t = sender_to(relay.addr());

    assert_eq!(relay.mute_existing(), 1, "the sender's flow is muted");
    // One second of sends at 30 a second into the silence: every one is
    // accepted into libsrt's send buffer and none is acknowledged, which is
    // what a lingering close waits for (about 3.5 s with the preset).
    for _ in 0..30 {
        t.send_bytes(&MSG)
            .expect("a send to a silent peer still succeeds");
        thread::sleep(Duration::from_millis(33));
    }

    let started = Instant::now();
    t.abandon();
    let took = started.elapsed();
    assert!(
        took < scaled(Duration::from_millis(1_500)),
        "abandon waited {took:?}: it lingered"
    );
    assert!(!t.is_alive());

    drop(relay);
    drop(release);
    accept.join();
}

#[test]
fn abandon_after_a_cancel_closes_without_error() {
    require_loopback!();
    let (port, accept, release) = held_peer();
    let mut t = sender_to(loopback(port));
    t.srt_cancel_handle().cancel();

    t.abandon();
    assert!(!t.is_alive());
    // `abandon` ends in `close`, whose own flag wins over the cancel latch.
    assert!(
        matches!(t.send_bytes(&MSG), Err(TransportError::Closed)),
        "a send after abandon reads Closed"
    );

    drop(release);
    accept.join();
}
