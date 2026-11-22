//! Silent-peer detection over real SRT sockets: the transport's reading of
//! the peer and its non-lingering close.
//!
//! A peer is made silent with the relay ([`crate::relay::Relay`]): it keeps
//! the connection open on both ends but forwards nothing, so the sender's
//! sends keep succeeding while nothing comes back.
//!
//! The managed-sender tests below decide on an ORDER, never on a time: when
//! the second connection delivers its first message, has the listener seen
//! the first connection end? A silent-peer trip re-dials about a window
//! after the mute; libsrt on the listener's side ends the muted connection
//! only about 5 s after it. Deadlines there are hang guards.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tst_core::transport::{Transport, TransportError};
use tst_pipeline::{
    BackoffStrategy, ManagedStatsHandle, ManagedTransport, RawSender, RawSenderConfig,
    ReconnectMode, ReconnectPolicy,
};
use tst_srt::shells::managed_raw_sender_from_url;
use tst_srt::{
    Passphrase, RecvError, ResourceCancel, SrtListenerBuilder, SrtSocket, SrtSocketConfig,
    SrtTransport, SrtUrl,
};
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

// ------------------------------------------------------------------------
// A managed sender opened with `x-silentpeertimeout`.
// ------------------------------------------------------------------------

/// The pace of every managed-sender test: 30 messages a second.
const PERIOD: Duration = Duration::from_millis(33);

/// How long any managed-sender test may wait for what it waits for: a hang
/// guard, never what a test asserts.
fn hang_guard() -> Duration {
    scaled(Duration::from_secs(12))
}

/// What the listener saw of one accepted connection.
#[derive(Debug, Default, Clone)]
struct Seen {
    first_message: Option<Instant>,
    /// Whether the FIRST connection had ended when this one delivered its
    /// first message: read in the same critical section that records that
    /// message, so the order cannot be skewed by when the test looks.
    first_had_ended: Option<bool>,
    ended: Option<Instant>,
    seqs: Vec<u32>,
}

/// A listener that accepts in a loop and records, per connection, when its
/// first message came, its sequence numbers, and when it saw it end.
struct Recorder {
    addr: SocketAddr,
    conns: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    listener_cancel: ResourceCancel,
    socket_cancels: Arc<Mutex<Vec<ResourceCancel>>>,
    accept_thread: Option<JoinHandle<()>>,
    recv_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Recorder {
    fn start(passphrase: Option<&str>) -> Self {
        let mut b = SrtListenerBuilder::new();
        // Short, so a receive thread looks at the stop flag often; a
        // timeout is not the end of a connection.
        b.recv_timeout(Duration::from_millis(200));
        if let Some(p) = passphrase {
            b.passphrase(Passphrase::new(p).expect("passphrase"));
        }
        let mut listener = b.bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("listener address");
        let listener_cancel = listener.cancel_handle();
        let conns = Arc::new(Mutex::new(Vec::<Seen>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let socket_cancels = Arc::new(Mutex::new(Vec::new()));
        let recv_threads = Arc::new(Mutex::new(Vec::new()));
        let accept_thread = {
            let (conns, stop, socket_cancels, recv_threads) = (
                Arc::clone(&conns),
                Arc::clone(&stop),
                Arc::clone(&socket_cancels),
                Arc::clone(&recv_threads),
            );
            thread::spawn(move || {
                while let Ok((mut sock, _)) = listener.accept() {
                    let idx = {
                        let mut c = conns.lock().unwrap();
                        c.push(Seen::default());
                        c.len() - 1
                    };
                    socket_cancels.lock().unwrap().push(sock.cancel_handle());
                    let (conns, stop) = (Arc::clone(&conns), Arc::clone(&stop));
                    let h = thread::spawn(move || {
                        let mut buf = [0u8; 1500];
                        while !stop.load(Ordering::SeqCst) {
                            match sock.recv(&mut buf) {
                                Ok(n) if n >= 4 => {
                                    let seq = u32::from_be_bytes(buf[..4].try_into().unwrap());
                                    let mut c = conns.lock().unwrap();
                                    let first_ended = c[0].ended.is_some();
                                    let seen = &mut c[idx];
                                    if seen.first_message.is_none() {
                                        seen.first_message = Some(Instant::now());
                                        seen.first_had_ended = Some(first_ended);
                                    }
                                    seen.seqs.push(seq);
                                }
                                Ok(_) | Err(RecvError::TimedOut) => {}
                                Err(_) => {
                                    conns.lock().unwrap()[idx].ended = Some(Instant::now());
                                    return;
                                }
                            }
                        }
                    });
                    recv_threads.lock().unwrap().push(h);
                }
            })
        };
        Self {
            addr,
            conns,
            stop,
            listener_cancel,
            socket_cancels,
            accept_thread: Some(accept_thread),
            recv_threads,
        }
    }

    fn snapshot(&self) -> Vec<Seen> {
        self.conns.lock().unwrap().clone()
    }

    /// Wait until connection `idx` has delivered a message.
    fn wait_first_message(&self, idx: usize, send: &mut dyn FnMut()) -> Vec<Seen> {
        let deadline = Instant::now() + hang_guard();
        loop {
            let seen = self.snapshot();
            if seen.get(idx).is_some_and(|c| c.first_message.is_some()) {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "hang guard: connection {idx} never delivered a message: {seen:?}"
            );
            send();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.listener_cancel.cancel();
        for c in self.socket_cancels.lock().unwrap().iter() {
            c.cancel();
        }
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        for h in self.recv_threads.lock().unwrap().drain(..) {
            let _ = h.join();
        }
    }
}

/// The policy a user of the key would run (an exponential backoff from
/// 100 ms to 1 s, a 256-message gap buffer that drops its oldest) with a
/// budget of five attempts instead of none: in Blocking mode the reconnect
/// runs inside the send, so a re-dial that can never succeed would keep the
/// send, and the test, waiting for ever. With a budget it ends as a failed
/// send within about 2.5 s, and [`Paced::send`] reports why. A healthy run
/// needs one attempt per outage.
fn users_policy(mode: ReconnectMode) -> ReconnectPolicy {
    ReconnectPolicy::default()
        .with_max_attempts(Some(5))
        .with_backoff(BackoffStrategy::Exponential {
            base: Duration::from_millis(100),
            max: Duration::from_secs(1),
        })
        .with_mode(mode)
}

struct Paced {
    tx: RawSender<ManagedTransport<SrtTransport>>,
    stats: ManagedStatsHandle,
    seq: u32,
}

impl Paced {
    fn open(addr: SocketAddr, query: &str, mode: ReconnectMode) -> Self {
        let url = SrtUrl::parse(&format!("srt://{addr}?{query}")).expect("parse");
        let (tx, _handles, stats) =
            managed_raw_sender_from_url(&url, users_policy(mode), RawSenderConfig::default())
                .expect("managed sender open");
        Self { tx, stats, seq: 0 }
    }

    /// One message carrying the next sequence number, then one period.
    fn send(&mut self) {
        let mut msg = MSG;
        msg[..4].copy_from_slice(&self.seq.to_be_bytes());
        self.seq += 1;
        if let Err(e) = self.tx.send(&msg) {
            panic!("a managed send failed ({e}); {}", failed_redials());
        }
        thread::sleep(PERIOD);
    }

    fn send_for(&mut self, d: Duration) {
        let until = Instant::now() + d;
        while Instant::now() < until {
            self.send();
        }
    }

    fn reconnect_attempts(&self) -> u64 {
        self.stats.stats().expect("stats").reconnect_attempts
    }

    fn reconnect_successes(&self) -> u64 {
        self.stats.stats().expect("stats").reconnect_successes
    }
}

/// The causes of the failed re-dials logged so far in this test binary, for
/// a failure message. Empty unless the test runs under `traced_test`. The
/// lines are not filtered by test: a Background worker's lines carry no
/// test's span, so a filter could hide the real cause. Under nextest each
/// test is its own process, so in practice the lines are this test's.
fn failed_redials() -> String {
    // `logs_assert` and `logs_contain` exist only inside the test function
    // that `traced_test` expands; this helper runs from `Paced::send`, so it
    // reads the buffer they read.
    let buf = tracing_test::internal::global_buf().lock().unwrap().clone();
    let causes: Vec<String> = String::from_utf8_lossy(&buf)
        .lines()
        .filter(|l| l.contains("reconnect attempt failed"))
        .map(str::to_string)
        .collect();
    format!("re-dial failures logged in this binary: {causes:?}")
}

/// The order every armed test asserts, decided when the second connection
/// delivered its first message; prints the margin when the first end is
/// already known (it is never waited for).
fn assert_second_connection_came_first(seen: &[Seen], muted: Instant) {
    let second = &seen[1];
    let first_msg = second
        .first_message
        .expect("second connection's first message");
    eprintln!(
        "second connection's first message {:?} after the mute; the listener had seen the first end by then: {:?}; first end {:?}",
        first_msg.duration_since(muted),
        second.first_had_ended,
        seen[0].ended.map(|e| e.duration_since(muted)),
    );
    assert_eq!(
        second.first_had_ended,
        Some(false),
        "the listener had already seen the first connection end: no trip came before libsrt's own break"
    );
}

fn reconnects_before_libsrt_breaks(mode: ReconnectMode) {
    require_loopback!();
    let rec = Recorder::start(None);
    let relay = Relay::start(rec.addr);
    let mut tx = Paced::open(relay.addr(), "x-silentpeertimeout=1000", mode);

    tx.send_for(Duration::from_secs(2));
    assert_eq!(relay.mute_existing(), 1);
    let muted = Instant::now();
    let seen = rec.wait_first_message(1, &mut || tx.send());
    assert_second_connection_came_first(&seen, muted);
    let deadline = Instant::now() + hang_guard();
    while tx.reconnect_successes() < 1 {
        assert!(
            Instant::now() < deadline,
            "hang guard: no reconnect counted"
        );
        tx.send();
    }
    tx.tx.close();
    drop(relay);
}

#[tracing_test::traced_test]
#[test]
fn an_armed_managed_sender_reconnects_long_before_libsrt_breaks_a_silent_peer() {
    reconnects_before_libsrt_breaks(ReconnectMode::Blocking);
}

#[tracing_test::traced_test]
#[test]
fn an_armed_managed_sender_reconnects_long_before_libsrt_breaks_a_silent_peer_in_background_mode() {
    reconnects_before_libsrt_breaks(ReconnectMode::Background);
}

#[tracing_test::traced_test]
#[test]
fn an_armed_encrypted_sender_does_not_trip_while_healthy_and_trips_when_muted() {
    require_loopback!();
    const SECRET: &str = "silent-peer-test-passphrase";
    let rec = Recorder::start(Some(SECRET));
    let relay = Relay::start(rec.addr);
    let mut tx = Paced::open(
        relay.addr(),
        &format!("passphrase={SECRET}&x-silentpeertimeout=1000"),
        ReconnectMode::Blocking,
    );

    tx.send_for(Duration::from_secs(3));
    assert_eq!(
        tx.reconnect_attempts(),
        0,
        "a healthy encrypted connection tripped"
    );
    assert_eq!(rec.snapshot().len(), 1);

    assert_eq!(relay.mute_existing(), 1);
    let muted = Instant::now();
    let seen = rec.wait_first_message(1, &mut || tx.send());
    assert_second_connection_came_first(&seen, muted);
    tx.tx.close();
    drop(relay);
}

#[tracing_test::traced_test]
#[test]
fn an_armed_sender_that_pauses_longer_than_the_window_does_not_reconnect() {
    require_loopback!();
    let rec = Recorder::start(None);
    let mut tx = Paced::open(
        rec.addr,
        "x-silentpeertimeout=1000",
        ReconnectMode::Blocking,
    );

    tx.send_for(Duration::from_secs(2));
    // An idle pause of three windows: input for the rule, not a wait.
    thread::sleep(Duration::from_secs(3));
    let first_after = tx.seq;
    for _ in 0..10 {
        tx.send();
    }
    let deadline = Instant::now() + hang_guard();
    loop {
        let seen = rec.snapshot();
        let delivered = (first_after..first_after + 10).all(|q| seen[0].seqs.contains(&q));
        if delivered {
            assert_eq!(seen.len(), 1, "a second connection was made: {seen:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "hang guard: the ten sends after the pause never all arrived: {seen:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        tx.reconnect_attempts(),
        0,
        "the pause made the sender reconnect"
    );
    tx.tx.close();
}

#[tracing_test::traced_test]
#[test]
fn an_unarmed_managed_sender_still_waits_for_libsrt() {
    require_loopback!();
    let rec = Recorder::start(None);
    let relay = Relay::start(rec.addr);
    let mut tx = Paced::open(relay.addr(), "", ReconnectMode::Blocking);

    tx.send_for(Duration::from_secs(2));
    assert_eq!(relay.mute_existing(), 1);
    let muted = Instant::now();
    // In Blocking mode the reconnect runs inside a send: the start of the
    // send after which a reconnect is counted is no later than the
    // reconnect itself, so it is a safe lower bound for when it happened.
    let deadline = Instant::now() + hang_guard();
    let reconnect_not_before = loop {
        let started = Instant::now();
        tx.send();
        if tx.reconnect_successes() >= 1 {
            break started.duration_since(muted);
        }
        assert!(
            Instant::now() < deadline,
            "hang guard: libsrt never broke the muted connection"
        );
    };
    assert!(
        reconnect_not_before >= Duration::from_millis(4_500),
        "an unarmed sender reconnected {reconnect_not_before:?} after the mute: it was armed"
    );
    tx.tx.close();
    drop(relay);
}
