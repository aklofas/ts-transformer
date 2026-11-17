//! `tst_<family>_peer_addr` and `tst_<family>_get_source_rejected` on the UDP
//! and RIST receivers: the address of the sender whose data was delivered
//! last (empty before the first), what a `?source=` pin turned away, and
//! that both answer while another thread holds the handle's slot and after
//! `_cancel`.
//!
//! The C RIST receiver has no local-port getter, so every RIST test binds a
//! port reserved for this file: 33140–33150, EVEN (the Simple profile puts
//! RTCP on `port + 1`), disjoint from the other RIST tests' ranges.
//!
//! The second sender of a two-address test sends from 127.0.0.2, which every
//! Linux host routes to the loopback. A UDP sender binds it directly; a RIST
//! sender cannot choose its source address, so it is relayed through a
//! two-way UDP relay whose outward socket is bound there. Only a host where
//! 127.0.0.2 cannot be bound skips those tests, never Linux.
//!
//! Every payload is seven null TS packets whose first payload byte carries
//! the sending instance's tag, so a test can tell from the bytes who
//! delivered them; the packets stay conformant and a demuxer makes no events
//! from them. Each test waits under one deadline, 12 s (scaled by
//! `TST_TEST_TIMEOUT_SCALE`), inside the network group's 20 s kill, so a
//! failure reports its own message. Waits are polls; a receive that would
//! park is guarded by a watchdog that cancels the handle at the deadline.

#![cfg(any(feature = "udp", feature = "rist"))]

use std::ffi::{CStr, CString};
#[cfg(feature = "rist")]
use std::net::IpAddr;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tstrans::error::TstError;
use tstrans::event::TstEvent;

use crate::common::{last_error_msg, scaled, ts_null_packet};
use crate::common_net::SendPtr;
#[cfg(feature = "udp")]
use crate::common_net::parked;

type PeerAddrFn<T> = unsafe extern "C" fn(*mut T, *mut c_char, usize) -> i32;
type RejectedFn<T> = unsafe extern "C" fn(*mut T, *mut u64) -> i32;
#[cfg(feature = "udp")]
type CancelFn<T> = unsafe extern "C" fn(*mut T) -> i32;

/// Packets a negative claim is judged over.
const ENOUGH: usize = 200;
const SECOND_V4: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const TAG_A: u8 = 0xA1;
const TAG_B: u8 = 0xB2;

/// The byte of each packet that carries the sender's tag: the first payload
/// byte of a null packet.
const TAG_BYTE: usize = 4;

/// The one deadline every wait of a test shares.
fn deadline() -> Instant {
    Instant::now() + scaled(Duration::from_secs(12))
}

/// Seven null TS packets whose first payload byte is `tag`.
fn tagged_chunk(tag: u8) -> Vec<u8> {
    let mut pkt = ts_null_packet();
    pkt[TAG_BYTE] = tag;
    pkt.repeat(7)
}

/// A thread that runs until this is dropped; `f` sees the stop flag.
struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(f: impl FnOnce(&AtomicBool) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || f(&flag));
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.take().map(JoinHandle::join);
    }
}

/// `peer_addr` into a 64-byte buffer: the return code and the text written.
fn peer<T>(h: *mut T, f: PeerAddrFn<T>) -> (i32, String) {
    let mut buf = [0x55 as c_char; 64];
    let rc = unsafe { f(h, buf.as_mut_ptr(), buf.len()) };
    if rc < 0 {
        return (rc, String::new());
    }
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .expect("UTF-8")
        .to_owned();
    assert_eq!(rc as usize, text.len(), "the return is the text's length");
    (rc, text)
}

fn rejected<T>(h: *mut T, f: RejectedFn<T>) -> u64 {
    let mut n = u64::MAX;
    assert_eq!(unsafe { f(h, &mut n) }, 0, "{}", last_error_msg());
    n
}

/// `(0, "")`: nothing delivered yet.
fn assert_empty<T>(h: *mut T, f: PeerAddrFn<T>, what: &str) {
    assert_eq!(peer(h, f), (0, String::new()), "{what}");
}

/// The address `peer_addr` reports, parsed.
fn peer_socket_addr<T>(h: *mut T, f: PeerAddrFn<T>, what: &str) -> SocketAddr {
    let (rc, text) = peer(h, f);
    assert!(rc > 0, "{what}: rc {rc}, {}", last_error_msg());
    text.parse()
        .unwrap_or_else(|e| panic!("{what}: {text:?} is not an address: {e}"))
}

/// Poll `peer_addr` until it reports an address; panics at the deadline.
fn wait_for_peer<T>(h: *mut T, f: PeerAddrFn<T>, deadline: Instant, what: &str) -> SocketAddr {
    loop {
        let (rc, _) = peer(h, f);
        if rc != 0 {
            return peer_socket_addr(h, f, what);
        }
        assert!(Instant::now() < deadline, "{what}: no address seen");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A 127.0.0.2 socket, or `None` with the reason printed where that address
/// cannot be bound; never on Linux.
fn second_address_socket() -> Option<UdpSocket> {
    match UdpSocket::bind((SECOND_V4, 0)) {
        Ok(s) => Some(s),
        Err(e) if cfg!(target_os = "linux") => panic!("no second loopback address: {e}"),
        Err(e) => {
            eprintln!("skipping: {SECOND_V4} cannot be bound here ({e})");
            None
        }
    }
}

/// Cancels the handle at the deadline unless dropped first, so a receive
/// that would park forever returns `TST_E_CLOSED` instead.
#[cfg(feature = "udp")]
fn watchdog<T: 'static>(h: *mut T, cancel: CancelFn<T>, deadline: Instant) -> Worker {
    let p = SendPtr(h);
    Worker::spawn(move |stop| {
        let p = p; // whole-struct capture: SendPtr is Send, its field is not
        while !stop.load(Ordering::Relaxed) {
            if Instant::now() >= deadline {
                unsafe { cancel(p.0) };
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })
}

/// A reader thread that pulls until the handle reports something other than
/// success or an empty poll (the cancel's `TST_E_CLOSED`), and returns it.
fn reader<T: 'static>(h: *mut T, pull: fn(*mut T) -> i32) -> JoinHandle<i32> {
    let p = SendPtr(h);
    std::thread::spawn(move || {
        let p = p;
        loop {
            let rc = pull(p.0);
            if rc != 0 && rc != TstError::BufferFull as i32 {
                return rc;
            }
        }
    })
}

/// NULL handle, NULL buffer, NULL out pointer and a zero-length buffer are
/// all `TST_E_INVALID_CONFIG`; nothing is written. `h` has no address yet.
fn assert_invalid_arguments<T>(h: *mut T, peer_f: PeerAddrFn<T>, rej_f: RejectedFn<T>) {
    let invalid = TstError::InvalidConfig as i32;
    let mut buf = [0x55 as c_char; 64];
    assert_eq!(
        unsafe { peer_f(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len()) },
        invalid
    );
    assert_eq!(unsafe { peer_f(h, std::ptr::null_mut(), 64) }, invalid);
    assert_eq!(unsafe { peer_f(h, buf.as_mut_ptr(), 0) }, invalid);
    assert!(
        last_error_msg().contains("buffer too small"),
        "{}",
        last_error_msg()
    );
    assert!(buf.iter().all(|&b| b == 0x55), "nothing is written");
    let mut n = 7u64;
    assert_eq!(unsafe { rej_f(std::ptr::null_mut(), &mut n) }, invalid);
    assert_eq!(n, 7, "nothing is written");
    assert_eq!(unsafe { rej_f(h, std::ptr::null_mut()) }, invalid);
}

/// With an address of `len` bytes recorded, a buffer of `len` (one short of
/// the NUL) is refused and nothing in a larger backing array changes; a
/// buffer of `len + 1` takes it.
fn assert_short_buffer_refused<T>(h: *mut T, f: PeerAddrFn<T>, len: usize) {
    let mut backing = [0x55 as c_char; 64];
    let rc = unsafe { f(h, backing.as_mut_ptr(), len) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
    assert!(
        last_error_msg().contains("buffer too small"),
        "{}",
        last_error_msg()
    );
    assert!(backing.iter().all(|&b| b == 0x55), "nothing is written");
    let rc = unsafe { f(h, backing.as_mut_ptr(), len + 1) };
    assert_eq!(rc as usize, len, "{}", last_error_msg());
    assert_eq!(backing[len], 0, "NUL-terminated");
    assert!(
        backing[len + 1..].iter().all(|&b| b == 0x55),
        "nothing past the NUL"
    );
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

#[cfg(feature = "udp")]
mod udp {
    use super::*;
    use tstrans::udp::*;

    /// A plain socket that sends one tagged chunk every 2 ms until dropped.
    pub(super) struct UdpTx {
        pub(super) addr: SocketAddr,
        _worker: Worker,
    }

    impl UdpTx {
        pub(super) fn start(sock: UdpSocket, port: u16, tag: u8) -> Self {
            let addr = sock.local_addr().unwrap();
            let _worker = Worker::spawn(move |stop| {
                let chunk = tagged_chunk(tag);
                while !stop.load(Ordering::Relaxed) {
                    sock.send_to(&chunk, ("127.0.0.1", port)).expect("send");
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            Self { addr, _worker }
        }

        pub(super) fn from_localhost(port: u16, tag: u8) -> Self {
            Self::start(UdpSocket::bind("127.0.0.1:0").unwrap(), port, tag)
        }
    }

    pub(super) fn open_rx(query: &str) -> (*mut TstUdpReceiver, u16) {
        let port = crate::common_net::free_udp_port();
        let url = CString::new(format!("udp://127.0.0.1:{port}{query}")).unwrap();
        let rx = unsafe { tst_udp_recv_open(url.as_ptr()) };
        assert!(!rx.is_null(), "open: {}", last_error_msg());
        (rx, port)
    }

    pub(super) fn open_demux_rx() -> (*mut TstUdpDemuxReceiver, u16) {
        let port = crate::common_net::free_udp_port();
        let url = CString::new(format!("udp://127.0.0.1:{port}")).unwrap();
        let rx = unsafe { tst_udp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        assert!(!rx.is_null(), "open: {}", last_error_msg());
        (rx, port)
    }

    /// Read packets until `done(tags so far)` holds; returns the tags. A
    /// watchdog cancels the handle at `deadline`.
    pub(super) fn read_until(
        rx: *mut TstUdpReceiver,
        deadline: Instant,
        what: &str,
        mut done: impl FnMut(&[u8]) -> bool,
    ) -> Vec<u8> {
        let _dog = watchdog(rx, tst_udp_receiver_cancel, deadline);
        let mut buf = [0u8; 188];
        let mut n = 0usize;
        let mut tags = Vec::new();
        while !done(&tags) {
            let rc = unsafe { tst_udp_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
            assert_eq!(
                rc,
                0,
                "{what}: {} after {} packets",
                last_error_msg(),
                tags.len()
            );
            assert_eq!(buf[0], 0x47, "{what}: a TS packet");
            tags.push(buf[TAG_BYTE]);
        }
        tags
    }

    pub(super) fn recv_ts(h: *mut TstUdpReceiver) -> i32 {
        let mut buf = [0u8; 188];
        let mut n = 0usize;
        unsafe { tst_udp_receiver_recv_ts(h, buf.as_mut_ptr(), buf.len(), &mut n) }
    }

    pub(super) fn next_event(h: *mut TstUdpDemuxReceiver) -> i32 {
        let mut ev = TstEvent::default();
        unsafe { tst_udp_demux_receiver_next_event(h, &mut ev) }
    }
}

#[cfg(feature = "udp")]
#[test]
fn udp_receiver_peer_addr_is_empty_then_the_sender() {
    use tstrans::udp::*;
    let (rx, port) = udp::open_rx("");
    assert_empty(rx, tst_udp_receiver_peer_addr, "before any datagram");
    // An empty result is still a NUL-terminated string.
    let mut one = [0x55 as c_char; 1];
    assert_eq!(
        unsafe { tst_udp_receiver_peer_addr(rx, one.as_mut_ptr(), one.len()) },
        0
    );
    assert_eq!(one[0], 0);

    let tx = udp::UdpTx::from_localhost(port, TAG_A);
    udp::read_until(rx, deadline(), "the first packet", |t| !t.is_empty());
    let want = tx.addr.to_string();
    assert_eq!(want, format!("127.0.0.1:{}", tx.addr.port()));
    assert_eq!(
        peer(rx, tst_udp_receiver_peer_addr),
        (want.len() as i32, want)
    );
    assert_eq!(rejected(rx, tst_udp_receiver_get_source_rejected), 0);
    drop(tx);
    unsafe { tst_udp_receiver_close(rx) };
}

#[cfg(feature = "udp")]
#[test]
fn udp_demux_receiver_peer_addr() {
    use tstrans::udp::*;
    let (rx, port) = udp::open_demux_rx();
    assert_empty(rx, tst_udp_demux_receiver_peer_addr, "before any datagram");
    // Null packets yield no events, so the pull parks until the cancel.
    let pull = reader(rx, udp::next_event);
    let tx = udp::UdpTx::from_localhost(port, TAG_A);
    let got = wait_for_peer(
        rx,
        tst_udp_demux_receiver_peer_addr,
        deadline(),
        "udp demux",
    );
    assert_eq!(got, tx.addr);
    assert_eq!(rejected(rx, tst_udp_demux_receiver_get_source_rejected), 0);
    drop(tx);
    assert_eq!(unsafe { tst_udp_demux_receiver_cancel(rx) }, 0);
    assert_eq!(pull.join().expect("reader"), TstError::Closed as i32);
    assert_eq!(
        peer_socket_addr(rx, tst_udp_demux_receiver_peer_addr, "after cancel"),
        got
    );
    unsafe { tst_udp_demux_receiver_close(rx) };
}

// ---------------------------------------------------------------------------
// RIST
// ---------------------------------------------------------------------------

#[cfg(feature = "rist")]
mod rist {
    use super::*;
    use tstrans::rist::*;

    /// Shortens librist's recovery buffer so the first block arrives quickly.
    pub(super) const BUFFER: &str = "buffer=100";

    /// A C RIST sender on its own thread that sends one tagged chunk every
    /// 5 ms until dropped.
    pub(super) fn sender(port: u16, tag: u8) -> Worker {
        let url = CString::new(format!("rist://127.0.0.1:{port}?{BUFFER}")).unwrap();
        let tx = unsafe { tst_rist_sender_open(url.as_ptr()) };
        assert!(!tx.is_null(), "sender open: {}", last_error_msg());
        let p = SendPtr(tx);
        Worker::spawn(move |stop| {
            let p = p; // whole-struct capture: SendPtr is Send, its field is not
            let chunk = tagged_chunk(tag);
            while !stop.load(Ordering::Relaxed) {
                let rc = unsafe { tst_rist_sender_send_ts(p.0, chunk.as_ptr(), chunk.len()) };
                assert!(
                    rc == 0 || rc == TstError::BufferFull as i32,
                    "send: {rc} {}",
                    last_error_msg()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            unsafe { tst_rist_sender_close(p.0) };
        })
    }

    /// A two-way UDP relay: a sender talks to `front_port` on 127.0.0.1;
    /// the receiver sees its datagrams come from the outward socket's
    /// address.
    pub(super) struct Relay {
        pub(super) front_port: u16,
        _worker: Worker,
    }

    impl Relay {
        pub(super) fn start(back: UdpSocket, to: SocketAddr) -> Self {
            let front = UdpSocket::bind("127.0.0.1:0").unwrap();
            let front_port = front.local_addr().unwrap().port();
            front
                .set_read_timeout(Some(Duration::from_millis(2)))
                .unwrap();
            back.set_read_timeout(Some(Duration::from_millis(1)))
                .unwrap();
            let _worker = Worker::spawn(move |stop| {
                let mut buf = vec![0u8; 65536];
                let mut sender = None;
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, from)) = front.recv_from(&mut buf) {
                        sender = Some(from);
                        let _ = back.send_to(&buf[..n], to);
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
                _worker,
            }
        }
    }

    pub(super) fn open_rx(port: u16, query: &str) -> *mut TstRistReceiver {
        let url = CString::new(format!("rist://@127.0.0.1:{port}?{BUFFER}{query}")).unwrap();
        let rx = unsafe { tst_rist_recv_open(url.as_ptr()) };
        assert!(!rx.is_null(), "open {port}: {}", last_error_msg());
        rx
    }

    pub(super) fn open_demux_rx(port: u16) -> *mut TstRistDemuxReceiver {
        let url = CString::new(format!("rist://@127.0.0.1:{port}?{BUFFER}")).unwrap();
        let rx = unsafe { tst_rist_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        assert!(!rx.is_null(), "open {port}: {}", last_error_msg());
        rx
    }

    /// Poll packets until `done(tags so far)` holds; returns the tags. Each
    /// pull is one ~100 ms librist poll, `TST_E_BUFFER_FULL` when empty.
    pub(super) fn read_until(
        rx: *mut TstRistReceiver,
        deadline: Instant,
        what: &str,
        mut done: impl FnMut(&[u8]) -> bool,
    ) -> Vec<u8> {
        let mut buf = [0u8; 188];
        let mut n = 0usize;
        let mut tags = Vec::new();
        while !done(&tags) {
            assert!(
                Instant::now() < deadline,
                "{what}: not seen; {} packets read",
                tags.len()
            );
            let rc = unsafe { tst_rist_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
            if rc == 0 {
                assert_eq!(buf[0], 0x47, "{what}: a TS packet");
                tags.push(buf[TAG_BYTE]);
            } else {
                assert_eq!(
                    rc,
                    TstError::BufferFull as i32,
                    "{what}: {}",
                    last_error_msg()
                );
            }
        }
        tags
    }

    pub(super) fn next_event(h: *mut TstRistDemuxReceiver) -> i32 {
        let mut ev = TstEvent::default();
        unsafe { tst_rist_demux_receiver_next_event(h, &mut ev) }
    }
}

#[cfg(feature = "rist")]
#[test]
fn rist_receiver_peer_addr() {
    use tstrans::rist::*;
    const PORT: u16 = 33_140;
    let rx = rist::open_rx(PORT, "");
    assert_empty(rx, tst_rist_receiver_peer_addr, "before any block");
    let tx = rist::sender(PORT, TAG_A);
    let tags = rist::read_until(rx, deadline(), "the first packet", |t| !t.is_empty());
    assert_eq!(tags[0], TAG_A);
    let got = peer_socket_addr(rx, tst_rist_receiver_peer_addr, "rist");
    assert_eq!(got.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST), "{got}");
    assert_ne!(got.port(), 0, "{got}");
    assert_eq!(rejected(rx, tst_rist_receiver_get_source_rejected), 0);
    drop(tx);
    assert_short_buffer_refused(rx, tst_rist_receiver_peer_addr, got.to_string().len());
    unsafe { tst_rist_receiver_close(rx) };
}

#[cfg(feature = "rist")]
#[test]
fn rist_demux_receiver_peer_addr() {
    use tstrans::rist::*;
    const PORT: u16 = 33_142;
    let rx = rist::open_demux_rx(PORT);
    assert_empty(rx, tst_rist_demux_receiver_peer_addr, "before any block");
    let pull = reader(rx, rist::next_event);
    let tx = rist::sender(PORT, TAG_A);
    let got = wait_for_peer(
        rx,
        tst_rist_demux_receiver_peer_addr,
        deadline(),
        "rist demux",
    );
    assert_eq!(got.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST), "{got}");
    assert_ne!(got.port(), 0, "{got}");
    assert_eq!(rejected(rx, tst_rist_demux_receiver_get_source_rejected), 0);
    drop(tx);
    assert_eq!(unsafe { tst_rist_demux_receiver_cancel(rx) }, 0);
    assert_eq!(pull.join().expect("reader"), TstError::Closed as i32);
    assert_eq!(
        peer_socket_addr(rx, tst_rist_demux_receiver_peer_addr, "after cancel"),
        got
    );
    unsafe { tst_rist_demux_receiver_close(rx) };
}

// ---------------------------------------------------------------------------
// The pin, one test per family
// ---------------------------------------------------------------------------

/// A stop condition for a pin test: true once `ENOUGH` packets have been read
/// after the first time `rejected()` was above zero, so every one of them was
/// read while the pin had already turned the second sender away.
fn enough_after_first_rejection(mut rejected: impl FnMut() -> u64) -> impl FnMut(&[u8]) -> bool {
    let mut from = None;
    move |tags| {
        if from.is_none() && rejected() > 0 {
            from = Some(tags.len());
        }
        from.is_some_and(|f| tags.len() - f >= ENOUGH)
    }
}

fn assert_all_a(tags: &[u8], what: &str) {
    assert!(
        tags.iter().all(|&t| t == TAG_A),
        "{what}: {} of {} packets are not A's",
        tags.iter().filter(|&&t| t != TAG_A).count(),
        tags.len()
    );
}

/// `?source=first` latches the first sender; a second sender from 127.0.0.2
/// is turned away and counted, and none of its packets are delivered.
#[cfg(feature = "udp")]
mod udp_pin {
    use super::*;
    use tstrans::udp::*;

    #[test]
    fn source_rejected_counts_under_a_pin() {
        let Some(second) = second_address_socket() else {
            return;
        };
        let deadline = deadline();
        let (rx, port) = udp::open_rx("?source=first");
        let a = udp::UdpTx::from_localhost(port, TAG_A);
        udp::read_until(rx, deadline, "A's first packet", |t| !t.is_empty());
        let b = udp::UdpTx::start(second, port, TAG_B);
        let done =
            enough_after_first_rejection(|| rejected(rx, tst_udp_receiver_get_source_rejected));
        let tags = udp::read_until(rx, deadline, "B turned away", done);
        assert_all_a(&tags, "udp");
        assert_eq!(
            peer_socket_addr(rx, tst_udp_receiver_peer_addr, "pinned"),
            a.addr
        );
        drop((a, b));
        unsafe { tst_udp_receiver_close(rx) };
    }
}

/// As `udp_pin`, over RIST: the second sender is relayed from 127.0.0.2.
#[cfg(feature = "rist")]
mod rist_pin {
    use super::*;
    use tstrans::rist::*;

    #[test]
    fn source_rejected_counts_under_a_pin() {
        const PORT: u16 = 33_144;
        let Some(back) = second_address_socket() else {
            return;
        };
        let deadline = deadline();
        let rx = rist::open_rx(PORT, "&source=first");
        let relay = rist::Relay::start(back, SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)));
        let a = rist::sender(PORT, TAG_A);
        rist::read_until(rx, deadline, "A's first packet", |t| !t.is_empty());
        let b = rist::sender(relay.front_port, TAG_B);
        let done =
            enough_after_first_rejection(|| rejected(rx, tst_rist_receiver_get_source_rejected));
        let tags = rist::read_until(rx, deadline, "B turned away", done);
        assert_all_a(&tags, "rist");
        let got = peer_socket_addr(rx, tst_rist_receiver_peer_addr, "pinned");
        assert_eq!(got.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST), "{got}");
        drop((a, b, relay));
        unsafe { tst_rist_receiver_close(rx) };
    }
}

// ---------------------------------------------------------------------------
// Both families
// ---------------------------------------------------------------------------

/// Run `get` on a helper thread while a receive is parked on `h`. A getter
/// that waited for the receive's slot would never return, so after 2 s the
/// handle is cancelled (which ends the receive and frees the slot) and the
/// test fails with a message instead of hanging until the runner's kill.
#[cfg(feature = "udp")]
fn answers_while_parked<T: 'static>(
    h: *mut T,
    cancel: CancelFn<T>,
    get: impl FnOnce(*mut T) + Send + 'static,
) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let p = SendPtr(h);
    let getter = std::thread::spawn(move || {
        let p = p; // whole-struct capture: SendPtr is Send, its field is not
        get(p.0);
        let _ = tx.send(());
    });
    match rx.recv_timeout(scaled(Duration::from_secs(2))) {
        Ok(()) => getter.join().expect("getter thread"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            // The getter's own assertion failed: report it.
            if let Err(e) = getter.join() {
                std::panic::resume_unwind(e);
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            unsafe { cancel(h) };
            let _ = getter.join();
            panic!("the getter waited for the parked receive");
        }
    }
}

/// Both getters answer while another thread is parked in a receive (holding
/// the handle's slot), and after `_cancel` they answer as before it.
///
/// UDP only: a RIST pull does not park. Each is one ~100 ms librist poll that
/// releases the slot at its end, so a getter that took the slot would still
/// answer within a tick and no timing bound here could tell it apart. For
/// RIST the property rests on the forwarders reading the construction-time
/// snapshot; after `_cancel` it is covered by `rist_demux_receiver_peer_addr`.
#[cfg(feature = "udp")]
#[test]
fn peer_addr_answers_while_recv_is_parked_and_after_cancel() {
    use tstrans::udp::*;
    // A receiver that has delivered: the address survives the cancel.
    let (rx, port) = udp::open_rx("");
    let tx = udp::UdpTx::from_localhost(port, TAG_A);
    udp::read_until(rx, deadline(), "the first packet", |t| !t.is_empty());
    drop(tx);
    let want = peer(rx, tst_udp_receiver_peer_addr).1;
    assert!(!want.is_empty(), "{}", last_error_msg());
    let expected = want.clone();
    let rc = parked(
        rx,
        |h| loop {
            // Drain what the sender left queued, then park.
            let rc = udp::recv_ts(h);
            if rc != 0 {
                return rc;
            }
        },
        move |h| {
            // Let the drain finish so the reader is parked, not reading.
            std::thread::sleep(Duration::from_millis(300));
            answers_while_parked(h, tst_udp_receiver_cancel, move |h| {
                assert_eq!(peer(h, tst_udp_receiver_peer_addr).1, expected);
                assert_eq!(rejected(h, tst_udp_receiver_get_source_rejected), 0);
            });
            unsafe { tst_udp_receiver_cancel(h) }
        },
        |h| unsafe { tst_udp_receiver_close(h) },
    );
    assert_eq!(rc, TstError::Closed as i32);
    assert_eq!(peer(rx, tst_udp_receiver_peer_addr).1, want);
    assert_eq!(rejected(rx, tst_udp_receiver_get_source_rejected), 0);
    unsafe { tst_udp_receiver_close(rx) };

    // A demux receiver that has delivered nothing: empty throughout, as its
    // TS byte counter reads zero throughout.
    let (rx, _) = udp::open_demux_rx();
    let rc = parked(
        rx,
        udp::next_event,
        |h| {
            answers_while_parked(h, tst_udp_demux_receiver_cancel, |h| {
                assert_empty(h, tst_udp_demux_receiver_peer_addr, "parked");
                assert_eq!(rejected(h, tst_udp_demux_receiver_get_source_rejected), 0);
            });
            unsafe { tst_udp_demux_receiver_cancel(h) }
        },
        |h| unsafe { tst_udp_demux_receiver_close(h) },
    );
    assert_eq!(rc, TstError::Closed as i32);
    assert_empty(rx, tst_udp_demux_receiver_peer_addr, "after cancel");
    assert_eq!(rejected(rx, tst_udp_demux_receiver_get_source_rejected), 0);
    let mut n = u64::MAX;
    assert_eq!(
        unsafe { tst_udp_demux_receiver_get_ts_bytes_demuxed(rx, &mut n) },
        0
    );
    assert_eq!(n, 0);
    unsafe { tst_udp_demux_receiver_close(rx) };
}

/// NULL handle, NULL `buf`, NULL `out`, `buf_len` 0 and a buffer one byte
/// short of the address plus its NUL are `TST_E_INVALID_CONFIG`, and nothing
/// is written past (or into) the buffer.
#[test]
fn null_and_short_buffers_are_invalid_config() {
    #[cfg(feature = "udp")]
    {
        use tstrans::udp::*;
        let (rx, port) = udp::open_rx("");
        assert_invalid_arguments(
            rx,
            tst_udp_receiver_peer_addr,
            tst_udp_receiver_get_source_rejected,
        );
        let tx = udp::UdpTx::from_localhost(port, TAG_A);
        udp::read_until(rx, deadline(), "udp: the first packet", |t| !t.is_empty());
        let len = tx.addr.to_string().len();
        assert_short_buffer_refused(rx, tst_udp_receiver_peer_addr, len);
        drop(tx);
        unsafe { tst_udp_receiver_close(rx) };

        let (rx, _) = udp::open_demux_rx();
        assert_invalid_arguments(
            rx,
            tst_udp_demux_receiver_peer_addr,
            tst_udp_demux_receiver_get_source_rejected,
        );
        unsafe { tst_udp_demux_receiver_close(rx) };
    }
    #[cfg(feature = "rist")]
    {
        use tstrans::rist::*;
        let rx = rist::open_rx(33_148, "");
        assert_invalid_arguments(
            rx,
            tst_rist_receiver_peer_addr,
            tst_rist_receiver_get_source_rejected,
        );
        unsafe { tst_rist_receiver_close(rx) };

        let rx = rist::open_demux_rx(33_150);
        assert_invalid_arguments(
            rx,
            tst_rist_demux_receiver_peer_addr,
            tst_rist_demux_receiver_get_source_rejected,
        );
        unsafe { tst_rist_demux_receiver_close(rx) };
    }
}
