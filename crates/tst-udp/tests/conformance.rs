//! WP-C1's transport conformance kit, run against loopback UDP (spec §3.5 /
//! §6: every transport crate pins the one-cancel-outcome contract). Shape =
//! `crates/tst-tcp/tests/conformance.rs` with a `std::net::UdpSocket` peer.
//!
//! Receiver factories bind port 0 and publish the port through a shared
//! cell so the kit's `feed` closure can aim a std socket at whichever
//! receiver the kit built last (the kit builds a fresh transport per row).
//! Test names carry `loopback` for the nextest `network` group.
//!
//! Send rows run with `SendPark::NextCall` (cancel first, then ONE send):
//! a UDP `send_bytes` never parks, so the `Loop` model would only spray the
//! sink until the cancel landed.
//!
//! `not_alive_after_broken`:
//! - send: `BrokenSource::Induce` — the factory raises `pkt_size` to
//!   70 000 so a 66 000-byte datagram passes the `TooLarge` guard and the
//!   kernel refuses it with `EMSGSIZE` on every platform (IPv4 UDP payload
//!   ceiling 65 507 B; Windows `WSAEMSGSIZE`), a non-transient
//!   `io::ErrorKind` → `Broken`, and the row asserts `!is_alive()`
//!   (`tests/cancel.rs::send_broken_loopback_latches_dead` is the twin);
//! - recv: `BrokenSource::NotProducible` — an unconnected datagram socket
//!   never reports a peer error on `recv` and the private socket is not
//!   reachable from `tests/`; the kit prints its visible skip line and the
//!   latch is pinned by `src/recv.rs::tests::broken_recv_latches_dead`.
//!   `NotProducible` also skips `peer_eof_is_not_a_cancel` — UDP has no
//!   peer-EOF concept at all, so there is nothing for that row to induce.

use std::net::UdpSocket;
use std::sync::{Arc, Mutex};

use tst_core::transport::Transport;
use tst_core::transport::conformance::{
    self as kit, BrokenSource, PostCloseKind, RecvOptions, RecvRow, SendOptions, SendPark, SendRow,
};
use tst_udp::{UdpRecvTransport, UdpTransport, UdpTransportBuilder};

/// IPv4 UDP payload ceiling: 65 535 − 20 (IP) − 8 (UDP). `UdpRecvTransport::
/// max_payload()` returns 65 535, so the `≥` row holds with headroom.
const UDP_RECV_CEILING: usize = 65_507;

/// A std sink the senders aim at; lives for the whole test so the port stays
/// bound (an unconnected sender ignores ICMP anyway — `UdpTransport`'s doc).
fn sink() -> (UdpSocket, u16) {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind sink");
    let port = s.local_addr().unwrap().port();
    (s, port)
}

#[test]
fn udp_send_contract_loopback() {
    let (_sink, port) = sink();
    kit::assert_send_rows(
        move || {
            // The knob setters return `&mut Self` while `build` consumes
            // `self`, so this cannot be one chained expression.
            let mut b =
                UdpTransportBuilder::from_url(&format!("udp://127.0.0.1:{port}")).expect("url");
            // Lets the Induce closure's 66 000-byte datagram past the
            // TooLarge guard so the kernel, not us, refuses it.
            b.pkt_size(70_000);
            b.build().expect("build UdpTransport")
        },
        BrokenSource::Induce(Box::new(|t: &mut UdpTransport| {
            // EMSGSIZE → Broken + latch; the row then asserts !is_alive().
            let _ = t.send_bytes(&[0x47u8; 66_000]);
        })),
        SendOptions {
            post_close: PostCloseKind::Closed,
            park: SendPark::NextCall,
        },
        SendRow::ALL,
    );
}

/// Shared "port of the receiver built most recently" cell + the two
/// closures the kit wants, kept together so both tests read the same shape.
fn recv_factory_and_feed() -> (impl Fn() -> UdpRecvTransport, impl Fn(&[u8])) {
    let current_port: Arc<Mutex<u16>> = Arc::new(Mutex::new(0));
    let port_for_factory = Arc::clone(&current_port);
    let port_for_feed = Arc::clone(&current_port);
    let factory = move || {
        let recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind UdpRecvTransport");
        *port_for_factory.lock().unwrap() = recv.local_addr().port();
        recv
    };
    let feed = move |payload: &[u8]| {
        let port = *port_for_feed.lock().unwrap();
        assert_ne!(port, 0, "feed called before any factory call");
        let s = UdpSocket::bind("127.0.0.1:0").expect("bind feeder");
        s.send_to(payload, ("127.0.0.1", port))
            .expect("feed datagram");
    };
    (factory, feed)
}

#[test]
fn udp_recv_contract_loopback() {
    let (factory, feed) = recv_factory_and_feed();
    kit::assert_recv_rows(
        factory,
        feed,
        BrokenSource::<UdpRecvTransport>::NotProducible,
        RecvOptions::default(),
        RecvRow::ALL,
    );
}

#[test]
fn udp_recv_max_payload_ge_ceiling_loopback() {
    let (factory, _feed) = recv_factory_and_feed();
    kit::recv_max_payload_ge_ceiling(&factory(), UDP_RECV_CEILING);
}
