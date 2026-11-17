//! A RIST receiver reports the address of the sender whose data it
//! delivers, and with `?source=` takes data from one address only.
//!
//! A tst-rist sender cannot choose its source address, so the second sender
//! of a two-address test is relayed through a two-way UDP relay whose outward
//! socket is bound to 127.0.0.2. Every Linux host routes all of 127/8 to the
//! loopback, so there the relay always opens. Where 127.0.0.2 cannot be
//! bound, the receiver binds `[::]` and the relay's outward socket `[::1]`:
//! the direct sender is seen as 127.0.0.1 and the relayed one as `::1`. Only
//! a host where both fail skips the two-address tests, never Linux.
//!
//! Each payload is seven TS packets whose byte 1 carries the sending
//! instance's tag, so a test can tell from the bytes who delivered them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_core::url::UrlError;
use tst_rist::{RistError, RistRecvTransport, RistTransport};

/// Every wait in this file.
const WAIT: Duration = Duration::from_secs(15);
/// Blocks a negative claim is judged over.
const ENOUGH: usize = 200;
/// Shortens librist's recovery buffer so the first block arrives quickly.
const BUFFER: &str = "buffer=100";
#[cfg(feature = "mbedtls")]
const KEY: &str = "secret=correct-horse-battery&aes-type=128";
#[cfg(feature = "mbedtls")]
const WRONG_KEY: &str = "secret=another-secret-entirely&aes-type=128";

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const SECOND_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

fn ts_chunk(tag: u8) -> Vec<u8> {
    let mut out = vec![0xffu8; 188 * 7];
    for pkt in out.chunks_mut(188) {
        pkt[0] = 0x47;
        pkt[1] = tag;
    }
    out
}

fn listen(query: &str) -> RistRecvTransport {
    listen_on("0.0.0.0", query)
}

fn listen_on(host: &str, query: &str) -> RistRecvTransport {
    let url = format!("rist://@{host}:0?{BUFFER}{query}");
    RistRecvTransport::listen(&url).unwrap_or_else(|e| panic!("{url}: {e:?}"))
}

/// A receiver and a relay whose datagrams reach it from a second address
/// (`Relay::second`). `None`, with the reason printed, only where neither way
/// works; never on Linux.
fn listen_with_relay(query: &str) -> Option<(RistRecvTransport, Relay)> {
    let v4 = UdpSocket::bind((SECOND_V4, 0));
    if let Ok(back) = v4 {
        let rx = listen_on("0.0.0.0", query);
        let to = SocketAddr::new(LOCALHOST, rx.local_addr().port());
        return Some((rx, Relay::start(back, to)));
    }
    // Windows does not take IPv4 on an IPv6 socket by default.
    let v6 = if cfg!(windows) {
        None
    } else {
        UdpSocket::bind("[::1]:0").ok()
    };
    if let Some(back) = v6 {
        let rx = listen_on("[::]", query);
        let to = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), rx.local_addr().port());
        return Some((rx, Relay::start(back, to)));
    }
    if cfg!(target_os = "linux") {
        panic!("no second loopback address: {v4:?}");
    }
    eprintln!("skipping: neither {SECOND_V4} nor [::1] can be bound here ({v4:?})");
    None
}

/// A sender on its own thread that sends one tagged chunk every 5 ms until
/// dropped.
struct Tx {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Tx {
    fn start(port: u16, query: &str, tag: u8) -> Self {
        let url = format!("rist://127.0.0.1:{port}?{BUFFER}{query}");
        let mut tx = RistTransport::connect(&url).unwrap_or_else(|e| panic!("{url}: {e:?}"));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let chunk = ts_chunk(tag);
            while !stopped.load(Ordering::Relaxed) {
                match tx.send_bytes(&chunk) {
                    Ok(()) | Err(TransportError::Backpressure { .. }) => {}
                    Err(e) => panic!("send: {e:?}"),
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A two-way UDP relay: a sender talks to `front_port` on 127.0.0.1; the
/// receiver sees its datagrams come from the outward socket's address,
/// 127.0.0.2 or, where that cannot be bound, `::1` (`Relay::second`).
struct Relay {
    front_port: u16,
    /// The address the receiver sees the relayed sender at.
    second: IpAddr,
    /// Datagrams relayed towards the receiver.
    forwarded: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Relay {
    /// Relay from a new front socket on 127.0.0.1 to `to`, out of `back`.
    fn start(back: UdpSocket, to: SocketAddr) -> Self {
        let second = back.local_addr().expect("back addr").ip();
        let front = UdpSocket::bind((LOCALHOST, 0)).expect("bind the relay's front");
        let front_port = front.local_addr().expect("front addr").port();
        front
            .set_read_timeout(Some(Duration::from_millis(2)))
            .expect("timeout");
        back.set_read_timeout(Some(Duration::from_millis(1)))
            .expect("timeout");
        let forwarded = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, stopped) = (forwarded.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            let mut sender = None;
            while !stopped.load(Ordering::Relaxed) {
                if let Ok((n, from)) = front.recv_from(&mut buf) {
                    sender = Some(from);
                    if back.send_to(&buf[..n], to).is_ok() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                }
                while let Ok((n, _)) = back.recv_from(&mut buf) {
                    if let Some(a) = sender {
                        let _ = front.send_to(&buf[..n], a);
                    }
                }
            }
        });
        Self {
            front_port,
            second,
            forwarded,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Read until `done(tags read so far, rx)` holds; returns the tags. Panics
/// at the deadline naming `what`.
fn read_until(
    rx: &mut RistRecvTransport,
    what: &str,
    mut done: impl FnMut(&[u8], &RistRecvTransport) -> bool,
) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    let mut buf = vec![0u8; rx.max_payload()];
    let mut tags = Vec::new();
    while !done(&tags, rx) {
        assert!(
            Instant::now() < deadline,
            "{what}: not seen within {WAIT:?}; {} blocks read, rejected {}",
            tags.len(),
            rx.stats().source_rejected
        );
        match rx.recv_bytes(&mut buf) {
            Ok(n) => {
                assert_eq!(n, 188 * 7, "{what}");
                tags.push(buf[1]);
            }
            Err(TransportError::Backpressure { .. }) => {}
            Err(e) => panic!("{what}: recv: {e:?}"),
        }
    }
    tags
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "{what}: not seen within {WAIT:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// An even port whose odd companion is free too (the Simple profile binds
/// both); same probe as `net_local_addr.rs`.
fn free_even_port() -> u16 {
    for _ in 0..100 {
        let probe = UdpSocket::bind("127.0.0.1:0").expect("bind probe");
        let port = probe.local_addr().expect("probe addr").port() & !1;
        drop(probe);
        if port == 0 {
            continue;
        }
        let pair = (
            UdpSocket::bind(("127.0.0.1", port)),
            UdpSocket::bind(("127.0.0.1", port + 1)),
        );
        if let (Ok(_), Ok(_)) = pair {
            return port;
        }
    }
    panic!("no free even port with a free odd neighbour in 100 tries");
}

fn ipv6_dual_stack_available() -> bool {
    !cfg!(windows) && UdpSocket::bind("[::1]:0").is_ok()
}

fn assert_reports_localhost(rx: &mut RistRecvTransport, query: &str, what: &str) {
    let tx = Tx::start(rx.local_addr().port(), query, 0xA1);
    let tags = read_until(rx, what, |t, _| !t.is_empty());
    assert_eq!(tags[0], 0xA1, "{what}");
    let peer = rx.peer_addr().unwrap_or_else(|| panic!("{what}: no peer"));
    assert_eq!(peer.ip(), LOCALHOST, "{what}: {peer}");
    assert_ne!(peer.port(), 0, "{what}: {peer}");
    drop(tx);
}

#[test]
fn main_profile_reports_the_senders_address() {
    let mut binds = vec!["rist://@127.0.0.1:0", "rist://@0.0.0.0:0"];
    // A dual-stack bind sees an IPv4 sender as ::ffff:127.0.0.1; the report
    // is the plain IPv4 address.
    if ipv6_dual_stack_available() {
        binds.push("rist://@[::]:0");
    }
    for bind in binds {
        let url = format!("{bind}?{BUFFER}");
        let mut rx = RistRecvTransport::listen(&url).unwrap_or_else(|e| panic!("{url}: {e:?}"));
        assert_reports_localhost(&mut rx, "", bind);
    }
}

#[cfg(feature = "mbedtls")]
#[test]
fn main_profile_reports_the_senders_address_with_a_key() {
    let mut rx = listen(&format!("&{KEY}"));
    assert_reports_localhost(&mut rx, &format!("&{KEY}"), "keyed");
}

#[test]
fn simple_profile_reports_the_senders_address() {
    let mut attempts = 0;
    let mut rx = loop {
        let url = format!(
            "rist://@127.0.0.1:{}?profile=simple&{BUFFER}",
            free_even_port()
        );
        match RistRecvTransport::listen(&url) {
            Ok(rx) => break rx,
            Err(RistError::PeerCreateFailed) if attempts < 10 => attempts += 1,
            Err(e) => panic!("{url}: {e:?}"),
        }
    };
    assert_reports_localhost(&mut rx, "&profile=simple", "simple");
}

#[test]
fn peer_addr_is_none_before_the_first_block() {
    let mut rx = listen("");
    let handle = rx
        .source_handle()
        .expect("a RIST receiver has a source handle");
    assert_eq!(rx.peer_addr(), None);
    assert_eq!(handle.peer_addr(), None);
    let _tx = Tx::start(rx.local_addr().port(), "", 0xA1);
    read_until(&mut rx, "first block", |t, _| !t.is_empty());
    assert_eq!(rx.peer_addr().map(|a| a.ip()), Some(LOCALHOST));
    assert_eq!(handle.peer_addr(), rx.peer_addr());
}

#[test]
fn an_unpinned_receiver_delivers_two_senders() {
    let Some((mut rx, relay)) = listen_with_relay("") else {
        return;
    };
    let port = rx.local_addr().port();
    let _a = Tx::start(port, "", 0xA1);
    let _b = Tx::start(relay.front_port, "", 0xB2);
    let tags = read_until(&mut rx, "both senders", |t, _| {
        t.contains(&0xA1) && t.contains(&0xB2)
    });
    assert!(tags.iter().all(|t| [0xA1, 0xB2].contains(t)));
    let peer = rx.peer_addr().expect("a peer");
    assert!([LOCALHOST, relay.second].contains(&peer.ip()), "{peer}");
    assert_eq!(rx.stats().source_rejected, 0);
}

/// Read until `ENOUGH` blocks and at least one refusal; every block is A's.
fn assert_only_a(rx: &mut RistRecvTransport, what: &str) {
    let tags = read_until(rx, what, |t, rx| {
        t.len() >= ENOUGH && rx.stats().source_rejected > 0
    });
    assert!(
        tags.iter().all(|&t| t == 0xA1),
        "{what}: {} of {} blocks are not A's",
        tags.iter().filter(|&&t| t != 0xA1).count(),
        tags.len()
    );
    assert_eq!(rx.peer_addr().map(|a| a.ip()), Some(LOCALHOST), "{what}");
}

#[test]
fn source_ip_keeps_the_second_sender_out() {
    let Some((mut rx, relay)) = listen_with_relay("&source=127.0.0.1") else {
        return;
    };
    let port = rx.local_addr().port();
    // B first, so the pin, not arrival order, keeps it out.
    let _b = Tx::start(relay.front_port, "", 0xB2);
    wait_for("B's datagrams through the relay", || {
        relay.forwarded.load(Ordering::Relaxed) > 0
    });
    let _a = Tx::start(port, "", 0xA1);
    assert_only_a(&mut rx, "source=127.0.0.1");
}

#[test]
fn source_first_keeps_the_second_sender_out() {
    let Some((mut rx, relay)) = listen_with_relay("&source=first") else {
        return;
    };
    let port = rx.local_addr().port();
    let _a = Tx::start(port, "", 0xA1);
    read_until(&mut rx, "A's first block", |t, _| !t.is_empty());
    let _b = Tx::start(relay.front_port, "", 0xB2);
    assert_only_a(&mut rx, "source=first");
}

/// The connect callback alone keeps B out: nothing is read, so the read
/// path never sees a block, yet B's datagrams are refused.
#[test]
fn source_ip_refuses_the_second_sender_before_its_data_is_read() {
    let Some((rx, relay)) = listen_with_relay("&source=127.0.0.1") else {
        return;
    };
    let _b = Tx::start(relay.front_port, "", 0xB2);
    wait_for("a refusal with nothing read", || {
        rx.stats().source_rejected > 0
    });
    assert_eq!(rx.peer_addr(), None);
}

/// The read path alone keeps the loser out: both senders are connected
/// before the first block latches `first`, so the connect callback accepted
/// both.
#[test]
fn source_first_refuses_a_sender_that_connected_before_the_latch() {
    let Some((mut rx, relay)) = listen_with_relay("&source=first") else {
        return;
    };
    let port = rx.local_addr().port();
    let _b = Tx::start(relay.front_port, "", 0xB2);
    let _a = Tx::start(port, "", 0xA1);
    wait_for("B sending for a while", || {
        relay.forwarded.load(Ordering::Relaxed) > 20
    });
    let tags = read_until(&mut rx, "one sender's blocks", |t, rx| {
        t.len() >= ENOUGH && rx.stats().source_rejected > 0
    });
    let winner = tags[0];
    assert!(
        tags.iter().all(|&t| t == winner),
        "{} of {} blocks are not the first sender's",
        tags.iter().filter(|&&t| t != winner).count(),
        tags.len()
    );
    let ip = if winner == 0xA1 {
        LOCALHOST
    } else {
        relay.second
    };
    assert_eq!(rx.peer_addr().map(|a| a.ip()), Some(ip));
}

#[cfg(feature = "mbedtls")]
#[test]
fn a_wrong_key_sender_first_does_not_take_the_lock() {
    let Some((mut rx, relay)) = listen_with_relay(&format!("&source=first&{KEY}")) else {
        return;
    };
    let port = rx.local_addr().port();
    let _b = Tx::start(relay.front_port, &format!("&{WRONG_KEY}"), 0xB2);
    // B runs for a second before A starts; nothing of B's can be delivered.
    let b_started = Instant::now();
    let mut buf = vec![0u8; rx.max_payload()];
    while b_started.elapsed() < Duration::from_secs(1) {
        match rx.recv_bytes(&mut buf) {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("only B is sending, with a wrong key: {other:?}"),
        }
    }
    assert!(relay.forwarded.load(Ordering::Relaxed) > 0, "B never sent");
    assert_eq!(rx.peer_addr(), None);
    let _a = Tx::start(port, &format!("&{KEY}"), 0xA1);
    let tags = read_until(&mut rx, "A's blocks", |t, _| t.len() >= ENOUGH);
    assert!(tags.iter().all(|&t| t == 0xA1));
    assert_eq!(rx.peer_addr().map(|a| a.ip()), Some(LOCALHOST));
}

#[test]
fn a_restarted_sender_on_a_new_port_continues() {
    let mut rx = listen("&source=first");
    let port = rx.local_addr().port();
    let first = Tx::start(port, "", 0xA1);
    read_until(&mut rx, "the first sender", |t, _| t.contains(&0xA1));
    drop(first);
    // A new sender on the same host: a new source port, so a new librist peer.
    let _second = Tx::start(port, "", 0xA2);
    read_until(&mut rx, "the restarted sender", |t, _| t.contains(&0xA2));
    assert_eq!(rx.peer_addr().map(|a| a.ip()), Some(LOCALHOST));
    assert_eq!(rx.stats().source_rejected, 0);
}

#[test]
fn a_sender_url_with_source_is_refused() {
    for value in ["127.0.0.1", "first"] {
        let url = format!("rist://127.0.0.1:9000?source={value}");
        let refusals = [
            RistTransport::connect(&url).err(),
            tst_rist::RistTransportBuilder::new(&url)
                .and_then(|b| b.connect())
                .err(),
        ];
        for e in refusals {
            match e {
                Some(RistError::InvalidConfig(m)) => {
                    assert!(m.contains("source") && m.contains("receive"), "{url}: {m}");
                }
                other => panic!("{url}: {other:?}"),
            }
        }
    }
}

#[test]
fn a_bad_source_value_is_refused_naming_the_key() {
    for value in ["camera.local", "", "First", "127.0.0.1:5000"] {
        let url = format!("rist://@127.0.0.1:0?source={value}");
        match RistRecvTransport::listen(&url) {
            Err(RistError::Url(UrlError::BadQueryValue { key, .. })) => {
                assert_eq!(key, "source", "{url}");
            }
            Err(e) => panic!("{url}: {e:?}"),
            Ok(_) => panic!("{url}: opened, expected a refusal"),
        }
    }
}

#[test]
fn source_handle_answers_while_a_receive_is_parked() {
    let mut rx = listen("");
    let port = rx.local_addr().port();
    let handle = rx
        .source_handle()
        .expect("a RIST receiver has a source handle");
    let cancel = rx.cancel_handle();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; rx.max_payload()];
        loop {
            match rx.recv_bytes(&mut buf) {
                Ok(_) | Err(TransportError::Backpressure { .. }) => {}
                Err(TransportError::ExplicitClose) => return,
                Err(e) => panic!("recv: {e:?}"),
            }
        }
    });
    assert_eq!(handle.peer_addr(), None);
    let _tx = Tx::start(port, "", 0xA1);
    wait_for("the handle's peer address", || handle.peer_addr().is_some());
    let peer: Option<SocketAddr> = handle.peer_addr();
    assert_eq!(peer.map(|a| a.ip()), Some(LOCALHOST));
    cancel.cancel();
    reader.join().expect("the reader ends at the cancel");
}
