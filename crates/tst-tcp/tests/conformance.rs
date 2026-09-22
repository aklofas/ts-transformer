//! WP-C1 — the tst-core transport conformance kit over plain `tcp://`
//! loopback pairs. The transport under test is the caller; the accepted
//! `std::net::TcpStream` is the silent peer (kept in `peer`, never read — so a
//! send loop parks against a 4 KiB send buffer within milliseconds;
//! `drop_peer` sends the FIN that is the `break_wire` for the
//! `not_alive_after_broken` and `peer_eof_is_not_a_cancel` rows). `tcps://`
//! rides the same `alive` / `cancelled` flags above `InnerStream`, so it is
//! not repeated here.
//!
//! `peer_eof_is_not_a_cancel` is the row the WP-C1 `TcpCancelHandle` split
//! exists for: the handle's `is_cancelled()` used to be `!alive`, and tst-tcp
//! drops `alive` on a clean peer EOF as well as on a cancel — so once
//! `Owned::is_cancelled` ORs the transport's latch in, every clean TCP EOF
//! would have been relabelled a caller close (`TST_E_CLOSED` for
//! `TST_E_END_OF_STREAM`).

use std::io::Write;
use std::net::{TcpListener as StdTcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use tst_core::transport::conformance::{self as kit, BrokenSource, RecvRow, SendPark, SendRow};
use tst_tcp::{SocketConfig, TcpTransport};

struct Pair {
    listener: StdTcpListener,
    port: u16,
    peer: Mutex<Option<TcpStream>>,
}

impl Pair {
    fn bind() -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        Self {
            listener,
            port,
            peer: Mutex::new(None),
        }
    }
    fn connect(&self) -> TcpTransport {
        let t = TcpTransport::connect(&format!("tcp://127.0.0.1:{}?sndbuf=4096", self.port))
            .expect("connect");
        let (peer, _) = self.listener.accept().expect("accept");
        *self.peer.lock().unwrap() = Some(peer);
        t
    }
    fn feed(&self, bytes: &[u8]) {
        self.peer
            .lock()
            .unwrap()
            .as_mut()
            .expect("a live peer")
            .write_all(bytes)
            .expect("peer write");
    }
    /// Drop the accepted peer: FIN — the clean end-of-stream a conformant
    /// receiver reports as a stream end, never as a caller cancel.
    fn drop_peer(&self) {
        self.peer.lock().unwrap().take();
    }
}

#[test]
fn tcp_send_contract_all_but_the_cancel_rows() {
    // `Arc` because `BrokenSource::Induce` boxes a `'static` closure: the
    // inducer cannot borrow a local `Pair`.
    let pair = Arc::new(Pair::bind());
    let breaker = Arc::clone(&pair);
    // Induce = drop the peer: FIN, then the kernel's RST on the next write →
    // EPIPE/ECONNRESET → Broken.
    kit::assert_send_rows(
        || pair.connect(),
        BrokenSource::Induce(Box::new(move |_t: &mut TcpTransport| breaker.drop_peer())),
        kit::SendOptions::default(),
        &SendRow::all_except(&[
            SendRow::CancelDuringParkIsExplicitClose,
            SendRow::CancelBeforeOpIsExplicitClose,
        ]),
    );
}

#[test]
#[ignore = "WP-C2 (Task C2.3): both cancel rows (during-park + before-op) return Closed on main — cancel drops `alive`, and the send/recv paths map `!alive` to Closed (tcp/transport.rs the mid-message and entry checks, and the recv loop's check). Un-ignore in PR 8."]
fn tcp_send_cancel_rows_are_explicit_close() {
    let pair = Pair::bind();
    kit::send_cancel_during_park_is_explicit_close(pair.connect(), SendPark::Loop);
    kit::send_cancel_before_op_is_explicit_close(pair.connect());
}

#[test]
fn tcp_recv_contract_all_but_the_cancel_rows() {
    let pair = Arc::new(Pair::bind());
    let breaker = Arc::clone(&pair);
    // Induce = drop the peer: FIN → `Ok(0)` → `Broken { cause: CleanEof }`.
    // `peer_eof_is_not_a_cancel` runs here, against that real FIN.
    kit::assert_recv_rows(
        || pair.connect(),
        |b| pair.feed(b),
        BrokenSource::Induce(Box::new(move |_r: &mut TcpTransport| breaker.drop_peer())),
        kit::RecvOptions::default(),
        &RecvRow::all_except(&[
            RecvRow::CancelDuringParkIsExplicitClose,
            RecvRow::CancelBeforeOpIsExplicitClose,
        ]),
    );
}

#[test]
#[ignore = "WP-C2 (Task C2.3): both cancel rows (during-park + before-op) return Closed on main — cancel drops `alive`, and the send/recv paths map `!alive` to Closed. Un-ignore in PR 8."]
fn tcp_recv_cancel_rows_are_explicit_close() {
    let pair = Pair::bind();
    kit::recv_cancel_during_park_is_explicit_close(pair.connect());
    kit::recv_cancel_before_op_is_explicit_close(pair.connect());
}

#[test]
fn tcp_recv_max_payload_ge_ceiling() {
    let pair = Pair::bind();
    // A stream has no protocol ceiling; the deliverable ceiling IS the
    // configured pkt_size (default 64 KiB).
    kit::recv_max_payload_ge_ceiling(&pair.connect(), SocketConfig::DEFAULT_PKT_SIZE);
}

/// The listener asymmetry the `tst_core::transport` table records, which the
/// kit has no row for (a `TcpListener` is neither a `Transport` nor a
/// `RecvTransport`, so no aggregate can reach it).
///
/// `TcpListener::close()` DOES latch `is_cancelled()` — a listener has no
/// peer-EOF path, so its only terminal event is the caller stopping it — while
/// `TcpTransport::close()` does NOT, because that transport's `alive` flag is
/// also dropped by a peer EOF and the two must stay distinguishable.
#[test]
fn listener_close_latches_is_cancelled_but_transport_close_does_not() {
    let listener = tst_tcp::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let h = listener.cancel_handle();
    assert!(!h.is_cancelled(), "a fresh listener handle");
    listener.close();
    assert!(
        h.is_cancelled(),
        "a listener's close() is a caller-initiated end and must latch"
    );

    let pair = Pair::bind();
    let mut t = pair.connect();
    let th = t.cancel_handle();
    assert!(!th.is_cancelled(), "a fresh transport handle");
    tst_core::transport::Transport::close(&mut t);
    assert!(
        !th.is_cancelled(),
        "a transport's own close() must NOT latch is_cancelled() — that bit \
         distinguishes a caller cancel from a peer EOF, and this transport's \
         liveness flag is dropped by both"
    );
}
