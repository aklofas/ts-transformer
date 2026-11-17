//! A receiver reports where its datagrams come from and, with `?source=`,
//! takes them from one address only.
//!
//! Two senders on different IPs are needed. Linux sends from `127.0.0.2`
//! to a receiver on `127.0.0.1`; where that address is not usable the
//! receiver binds `[::]` (dual stack) and the second sender is `[::1]`.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::SourcePin;
use tst_core::transport::{RecvTransport, TransportError};
use tst_core::url::UrlError;
use tst_udp::{UdpConfig, UdpError, UdpRecvTransport, UdpTransport, UdpTransportBuilder, UdpUrl};

/// Longest any wait in these tests lasts before the test fails.
const BOUND: Duration = Duration::from_secs(20);

struct Rig {
    /// Receiver URL host part, e.g. `127.0.0.1`.
    bind: &'static str,
    a: UdpSocket,
    b: UdpSocket,
    b_is_v6: bool,
}

impl Rig {
    /// `None` only where the platform has no second sender address.
    fn new() -> Option<Rig> {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        if let Ok(b) = UdpSocket::bind("127.0.0.2:0") {
            return Some(Rig {
                bind: "127.0.0.1",
                a,
                b,
                b_is_v6: false,
            });
        }
        if let Ok(b) = UdpSocket::bind("[::1]:0") {
            return Some(Rig {
                bind: "[::]",
                a,
                b,
                b_is_v6: true,
            });
        }
        if cfg!(target_os = "linux") {
            panic!("neither 127.0.0.2 nor ::1 can be bound on Linux");
        }
        eprintln!("skipped: no second loopback sender address on this platform");
        None
    }

    fn recv(&self, query: &str) -> UdpRecvTransport {
        UdpRecvTransport::listen(&format!("udp://@{}:0{query}", self.bind)).expect("bind")
    }

    fn a_ip(&self) -> &'static str {
        "127.0.0.1"
    }

    fn a_addr(&self) -> SocketAddr {
        self.a.local_addr().unwrap()
    }

    fn b_addr(&self) -> SocketAddr {
        self.b.local_addr().unwrap()
    }

    fn send_a(&self, to: SocketAddr, data: &[u8]) {
        self.a.send_to(data, loopback_v4(to)).unwrap();
    }

    fn send_b(&self, to: SocketAddr, data: &[u8]) {
        let dst = if self.b_is_v6 {
            SocketAddr::new(IpAddr::V6("::1".parse().unwrap()), to.port())
        } else {
            loopback_v4(to)
        };
        self.b.send_to(data, dst).unwrap();
    }
}

fn loopback_v4(to: SocketAddr) -> SocketAddr {
    SocketAddr::new(IpAddr::V4("127.0.0.1".parse().unwrap()), to.port())
}

/// Cancels `h` after `BOUND` unless dropped first, so a lost datagram fails a
/// test instead of hanging it.
struct Watchdog {
    done: Option<mpsc::Sender<()>>,
    fired: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Watchdog {
    fn arm(h: tst_udp::UdpCancelHandle) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let fired = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&fired);
        let join = thread::spawn(move || {
            if rx.recv_timeout(BOUND) == Err(mpsc::RecvTimeoutError::Timeout) {
                f.store(true, Ordering::SeqCst);
                h.cancel();
            }
        });
        Self {
            done: Some(tx),
            fired,
            join: Some(join),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.done.take());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // A second panic while unwinding would abort the whole test binary.
        if !thread::panicking() {
            assert!(!self.fired.load(Ordering::SeqCst), "watchdog fired");
        }
    }
}

fn recv_one(t: &mut UdpRecvTransport) -> Vec<u8> {
    let mut buf = vec![0u8; 65535];
    let n = t.recv_bytes(&mut buf).expect("recv_bytes");
    buf.truncate(n);
    buf
}

#[test]
fn peer_addr_is_none_before_the_first_datagram_then_the_senders_address() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv("");
    let _wd = Watchdog::arm(t.cancel_handle());
    assert_eq!(t.peer_addr(), None);
    rig.send_a(t.local_addr(), b"A1");
    assert_eq!(recv_one(&mut t), b"A1");
    assert_eq!(t.peer_addr(), Some(rig.a_addr()));
}

#[test]
fn an_unpinned_receiver_delivers_both_senders_and_reports_the_last() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv("");
    let _wd = Watchdog::arm(t.cancel_handle());
    let to = t.local_addr();
    rig.send_a(to, b"A");
    assert_eq!(recv_one(&mut t), b"A");
    rig.send_b(to, b"B");
    assert_eq!(recv_one(&mut t), b"B");
    assert_eq!(t.peer_addr().map(|a| a.ip()), Some(rig.b_addr().ip()));
    assert_eq!(t.stats().source_rejected, 0);
    assert_eq!(t.stats().datagrams_received, 2);
}

#[test]
fn source_ip_drops_and_counts_the_other_sender() {
    let Some(rig) = Rig::new() else { return };
    let t = rig.recv(&format!("?source={}", rig.a_ip()));
    run_interleaved(&rig, t);
}

#[test]
fn source_first_latches_on_the_first_sender() {
    let Some(rig) = Rig::new() else { return };
    let t = rig.recv("?source=first");
    run_interleaved(&rig, t);
}

/// 250 datagrams from A with 50 from B between them. A's first is delivered
/// before any of B's is sent (so a `first` pin latches on A); one thread
/// sends the rest in order, so every B datagram is queued before A's last.
fn run_interleaved(rig: &Rig, mut t: UdpRecvTransport) {
    let _wd = Watchdog::arm(t.cancel_handle());
    let to = t.local_addr();
    rig.send_a(to, b"A");
    assert_eq!(recv_one(&mut t), b"A");
    thread::scope(|s| {
        s.spawn(|| {
            for i in 0..249u32 {
                if i % 5 == 0 {
                    rig.send_b(to, b"B");
                }
                rig.send_a(to, b"A");
                thread::sleep(Duration::from_micros(300));
            }
        });
        for _ in 0..249 {
            assert_eq!(recv_one(&mut t), b"A");
        }
    });
    let st = t.stats();
    assert_eq!(st.datagrams_received, 250);
    assert_eq!(st.source_rejected, 50);
}

#[test]
fn a_pinned_sender_on_a_new_port_continues_and_peer_addr_follows() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv(&format!("?source={}", rig.a_ip()));
    let _wd = Watchdog::arm(t.cancel_handle());
    let to = t.local_addr();
    rig.send_a(to, b"one");
    assert_eq!(recv_one(&mut t), b"one");
    assert_eq!(t.peer_addr(), Some(rig.a_addr()));
    let a2 = UdpSocket::bind("127.0.0.1:0").unwrap();
    a2.send_to(b"two", loopback_v4(to)).unwrap();
    assert_eq!(recv_one(&mut t), b"two");
    assert_eq!(t.peer_addr(), Some(a2.local_addr().unwrap()));
    assert_ne!(rig.a_addr().port(), a2.local_addr().unwrap().port());
    assert_eq!(t.stats().source_rejected, 0);
}

#[test]
fn a_zero_length_probe_does_not_take_the_first_lock() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv("?source=first");
    let _wd = Watchdog::arm(t.cancel_handle());
    let to = t.local_addr();
    rig.send_b(to, b"");
    rig.send_a(to, b"A1");
    assert_eq!(recv_one(&mut t), b"A1");
    assert_eq!(t.peer_addr(), Some(rig.a_addr()));
    // The lock is A's: B is refused now.
    rig.send_b(to, b"B1");
    rig.send_a(to, b"A2");
    assert_eq!(recv_one(&mut t), b"A2");
    assert_eq!(t.stats().source_rejected, 1);
    // B's probe (unlatched, allowed), A1 and A2 are received; B1 is not.
    assert_eq!(t.stats().datagrams_received, 3);
}

#[test]
fn a_zero_length_datagram_from_the_wrong_sender_is_rejected_not_received() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv(&format!("?source={}", rig.a_ip()));
    let _wd = Watchdog::arm(t.cancel_handle());
    let to = t.local_addr();
    rig.send_b(to, b"");
    rig.send_a(to, b"A");
    assert_eq!(recv_one(&mut t), b"A");
    let st = t.stats();
    assert_eq!(st.source_rejected, 1);
    assert_eq!(st.datagrams_received, 1);
    assert_eq!(st.bytes_received, 1);
}

#[test]
fn recv_timeout_returns_none_at_its_deadline_under_a_flood_of_rejected_datagrams() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv(&format!("?source={}", rig.a_ip()));
    let to = t.local_addr();
    let deadline = Duration::from_millis(300);
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                rig.send_b(to, b"B");
            }
        });
        let mut buf = vec![0u8; 65535];
        let began = Instant::now();
        let r = t.recv_timeout(&mut buf, deadline);
        let took = began.elapsed();
        stop.store(true, Ordering::Relaxed);
        assert_eq!(r.unwrap(), None);
        assert!(took >= deadline, "returned early: {took:?}");
        // The documented bound is twice the deadline; the flood keeps the
        // socket busy, so the call ends at the deadline itself.
        assert!(took < deadline * 2 + Duration::from_secs(1), "{took:?}");
        assert!(t.stats().source_rejected > 0);
    });
}

#[test]
fn a_dual_stack_receiver_matches_ipv4_text() {
    let mut t = match UdpRecvTransport::listen("udp://@[::]:0?source=127.0.0.1") {
        Ok(t) => t,
        Err(e) if cfg!(target_os = "linux") => panic!("no IPv6 wildcard bind on Linux: {e}"),
        Err(e) => {
            eprintln!("skipped: no IPv6 wildcard bind: {e}");
            return;
        }
    };
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    a.send_to(b"4", ("127.0.0.1", t.local_addr().port()))
        .unwrap();
    let mut buf = vec![0u8; 65535];
    // Returns as soon as the datagram is there; the bound only matters when
    // the platform does not deliver IPv4 to a `[::]` bind.
    match t.recv_timeout(&mut buf, BOUND).unwrap() {
        Some(n) => assert_eq!(&buf[..n], b"4"),
        None if cfg!(target_os = "linux") => panic!("IPv4 was not delivered to a [::] bind"),
        None => {
            eprintln!("skipped: IPv4 is not delivered to a [::] bind here");
            return;
        }
    }
    assert_eq!(t.peer_addr(), Some(a.local_addr().unwrap()));
    assert_eq!(t.stats().source_rejected, 0);
}

#[test]
fn cancel_is_seen_under_a_flood_of_rejected_datagrams() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv(&format!("?source={}", rig.a_ip()));
    let to = t.local_addr();
    let cancel = t.cancel_handle();
    let handle = t.source_handle().expect("source handle");
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                rig.send_b(to, b"B");
            }
        });
        let (tx, rx) = mpsc::channel();
        s.spawn(move || {
            let mut buf = vec![0u8; 65535];
            let _ = tx.send(t.recv_bytes(&mut buf));
        });
        let start = Instant::now();
        while handle.rejected() == 0 {
            assert!(start.elapsed() < BOUND, "no datagram was rejected");
            thread::sleep(Duration::from_millis(5));
        }
        let cancelled_at = Instant::now();
        cancel.cancel();
        let r = rx.recv_timeout(Duration::from_secs(2));
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            r.expect("recv_bytes did not return within 2 s of cancel"),
            Err(TransportError::ExplicitClose)
        );
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
    });
}

#[test]
fn recv_timeout_reports_the_source_and_honours_the_pin() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv(&format!("?source={}", rig.a_ip()));
    let to = t.local_addr();
    let mut buf = vec![0u8; 65535];
    // Only the other sender: the call ends with None once its deadline passes.
    rig.send_b(to, b"B");
    let began = Instant::now();
    let r = t
        .recv_timeout(&mut buf, Duration::from_millis(300))
        .unwrap();
    assert_eq!(r, None);
    assert!(began.elapsed() >= Duration::from_millis(300));
    assert!(began.elapsed() < Duration::from_secs(5));
    assert_eq!(t.stats().source_rejected, 1);
    assert_eq!(t.stats().datagrams_received, 0);
    assert_eq!(t.peer_addr(), None);
    // Then the pinned sender is delivered.
    rig.send_b(to, b"B");
    rig.send_a(to, b"A");
    let n = t
        .recv_timeout(&mut buf, Duration::from_secs(10))
        .unwrap()
        .expect("A's datagram");
    assert_eq!(&buf[..n], b"A");
    assert_eq!(t.peer_addr(), Some(rig.a_addr()));
    assert_eq!(t.stats().source_rejected, 2);
}

#[test]
fn a_sender_url_with_source_is_refused() {
    let err = UdpTransport::connect("udp://127.0.0.1:9?source=127.0.0.1")
        .err()
        .expect("refused");
    assert!(
        matches!(&err, UdpError::Url(UrlError::Rejected(m)) if m.contains("source")),
        "{err:?}"
    );
    let err = UdpTransportBuilder::from_url("udp://127.0.0.1:9?source=first").expect_err("refused");
    assert!(
        matches!(&err, UrlError::Rejected(m) if m.contains("source")),
        "{err:?}"
    );
    // A hand-built config reaches the same refusal.
    let url = UdpUrl::parse("udp://127.0.0.1:9").unwrap();
    let mut cfg = UdpConfig::default();
    cfg.source = Some(SourcePin::First);
    let err = UdpTransport::with_config(&url, &cfg)
        .err()
        .expect("refused");
    assert!(
        matches!(&err, UdpError::Url(UrlError::Rejected(m)) if m.contains("source")),
        "{err:?}"
    );
}

#[test]
fn a_receive_url_given_to_a_sender_is_refused_for_its_at_prefix_first() {
    let url = "udp://@127.0.0.1:9?source=first";
    let a = UdpTransport::connect(url).err().expect("refused");
    let b = UdpTransportBuilder::from_url(url).expect_err("refused");
    assert!(a.to_string().contains("'@' prefix"), "{a}");
    assert!(b.to_string().contains("'@' prefix"), "{b}");
}

#[test]
fn a_pin_that_can_never_match_is_refused_at_open() {
    // An IPv6 pin on an IPv4 bind, plain or as the IPv4-mapped form's
    // opposite, can match nothing.
    for url in [
        "udp://@127.0.0.1:0?source=::1",
        "udp://@127.0.0.1:0?source=fe80::1",
    ] {
        let err = UdpRecvTransport::listen(url).err().expect(url);
        assert!(err.to_string().contains("source"), "{url}: {err}");
    }
    // The mapped form of an IPv4 address is that address.
    UdpRecvTransport::listen("udp://@127.0.0.1:0?source=::ffff:127.0.0.1").expect("mapped pin");
    // An IPv4 pin on an IPv6 bind stays allowed: a dual-stack bind delivers IPv4.
    match UdpRecvTransport::listen("udp://@[::]:0?source=127.0.0.1") {
        Ok(_) => {}
        Err(UdpError::Io(e)) if !cfg!(target_os = "linux") => eprintln!("no [::] bind: {e}"),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn source_handle_shows_whether_a_pin_is_in_force() {
    let t = UdpRecvTransport::listen("udp://@127.0.0.1:0?sourse=127.0.0.1").unwrap();
    assert_eq!(t.source_handle().unwrap().pin(), None);
    let t = UdpRecvTransport::listen("udp://@127.0.0.1:0?source=first").unwrap();
    assert_eq!(t.source_handle().unwrap().pin(), Some(SourcePin::First));
}

#[test]
fn a_bad_source_value_is_refused_naming_the_key() {
    let err = UdpRecvTransport::listen("udp://@127.0.0.1:0?source=somehost")
        .err()
        .expect("refused");
    match err {
        UdpError::Url(UrlError::BadQueryValue { key, value, .. }) => {
            assert_eq!(key, "source");
            assert_eq!(value, "somehost");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn source_handle_answers_while_a_receive_is_parked() {
    let Some(rig) = Rig::new() else { return };
    let mut t = rig.recv("");
    let to = t.local_addr();
    let handle = t.source_handle().expect("source handle");
    assert_eq!(handle.peer_addr(), None);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let join = thread::spawn(move || {
        let mut buf = vec![0u8; 65535];
        entered_tx.send(()).unwrap();
        let _ = tx.send(t.recv_bytes(&mut buf));
    });
    entered_rx
        .recv_timeout(BOUND)
        .expect("receive thread started");
    // Nothing was sent, so the receive stays parked across a poll tick; the
    // read below happens while it is.
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(300)),
        Err(mpsc::RecvTimeoutError::Timeout),
        "the receive returned without a datagram"
    );
    assert_eq!(handle.peer_addr(), None);
    rig.send_a(to, b"A");
    assert_eq!(rx.recv_timeout(BOUND).expect("first datagram"), Ok(1));
    // The address is stored before the receive returns, so this read does
    // not race anything.
    assert_eq!(handle.peer_addr(), Some(rig.a_addr()));
    join.join().unwrap();
}
