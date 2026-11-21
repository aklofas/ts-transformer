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
struct HeldPeer {
    port: u16,
    // Declared before `accept`, so a test that panics releases the peer
    // first and the accept thread's join does not wait out its hold.
    release: mpsc::Sender<()>,
    accept: AcceptHandle<()>,
    accepted: mpsc::Receiver<()>,
}

fn held_peer() -> HeldPeer {
    let lb = Loopback::bind();
    let port = lb.port;
    let (release, released) = mpsc::channel::<()>();
    let (accepted_tx, accepted) = mpsc::channel::<()>();
    let accept = lb.spawn_accept(move |peer| {
        let _ = accepted_tx.send(());
        let _ = released.recv_timeout(scaled(Duration::from_secs(15)));
        drop(peer);
    });
    accept.wait_ready();
    HeldPeer {
        port,
        accept,
        release,
        accepted,
    }
}

impl HeldPeer {
    /// Wait until the listener holds the connection. A caller that closed
    /// before the accept could leave it with nothing to accept.
    fn wait_accepted(&self) {
        self.accepted
            .recv_timeout(scaled(Duration::from_secs(10)))
            .expect("the listener never accepted the connection");
    }

    fn finish(self) {
        drop(self.release);
        self.accept.join();
    }
}

/// An SRT sender with the sender preset (5 s linger), dialled to `addr`,
/// once `peer` holds the connection.
fn sender_to(addr: SocketAddr, peer: &HeldPeer) -> SrtTransport {
    let socket =
        SrtSocket::connect_with(&SrtSocketConfig::sender_defaults(), addr).expect("connect");
    peer.wait_accepted();
    SrtTransport::new(socket)
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn peer_activity_moves_once_the_peer_acknowledges() {
    require_loopback!();
    let peer = held_peer();
    let mut t = sender_to(loopback(peer.port), &peer);

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
    peer.finish();
}

#[test]
fn peer_activity_is_none_once_closed() {
    require_loopback!();
    let peer = held_peer();
    let mut t = sender_to(loopback(peer.port), &peer);
    t.close();
    assert_eq!(t.peer_activity(), None);
    peer.finish();
}

#[test]
fn abandon_does_not_wait_for_unacknowledged_data() {
    require_loopback!();
    let peer = held_peer();
    let relay = Relay::start(loopback(peer.port));
    let mut t = sender_to(relay.addr(), &peer);

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
    peer.finish();
}

#[test]
fn a_normal_close_still_lingers_on_unacknowledged_data() {
    require_loopback!();
    let peer = held_peer();
    let relay = Relay::start(loopback(peer.port));
    // The preset with a 2 s linger instead of 5 s: the same close path, a
    // shorter test.
    let mut cfg = SrtSocketConfig::sender_defaults();
    cfg.linger = Some(Duration::from_secs(2));
    let socket = SrtSocket::connect_with(&cfg, relay.addr()).expect("connect");
    peer.wait_accepted();
    let mut t = SrtTransport::new(socket);

    assert_eq!(relay.mute_existing(), 1, "the sender's flow is muted");
    let muted = Instant::now();
    // A few sends into the silence: accepted into libsrt's send buffer and
    // never acknowledged, which is what a lingering close waits for.
    for _ in 0..5 {
        t.send_bytes(&MSG)
            .expect("a send to a silent peer still succeeds");
    }

    // Only `abandon` turns linger off. With linger on, libsrt's close waits
    // while unsent data is held and the connection is not broken, until
    // the linger time has passed since the close (2 s here); libsrt breaks
    // the connection only after 5 s without a packet from the peer, and the
    // last one came at most about a second before the mute (an idle
    // connection hears a keepalive about once a second). Either way a
    // lingering close returns at least 2 s after the mute, and timers that
    // fire late on a slow machine only make that later. A close without
    // linger returns as soon as the five sends are done: milliseconds here,
    // still far under the bound on a machine ten times slower.
    t.close();
    let since_mute = muted.elapsed();
    assert!(
        since_mute >= Duration::from_millis(1_500),
        "close returned {since_mute:?} after the mute: it did not linger"
    );

    drop(relay);
    peer.finish();
}

#[test]
fn peer_activity_is_none_once_cancelled() {
    require_loopback!();
    let peer = held_peer();
    let t = sender_to(loopback(peer.port), &peer);
    assert!(t.peer_activity().is_some());
    // The cancel closes the socket but leaves it in the transport's slot,
    // so the reading asks libsrt, which refuses statistics on it.
    t.srt_cancel_handle().cancel();
    assert_eq!(t.peer_activity(), None, "a cancelled socket has no reading");
    drop(t);
    peer.finish();
}

#[test]
fn abandon_after_a_cancel_closes_without_error() {
    require_loopback!();
    let peer = held_peer();
    let mut t = sender_to(loopback(peer.port), &peer);
    t.srt_cancel_handle().cancel();

    t.abandon();
    assert!(!t.is_alive());
    // `abandon` ends in `close`, whose own flag wins over the cancel latch.
    assert!(
        matches!(t.send_bytes(&MSG), Err(TransportError::Closed)),
        "a send after abandon reads Closed"
    );

    peer.finish();
}
