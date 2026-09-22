//! `SrtUrl::connect` / `SrtUrl::accept_one` — the one open path every
//! binding composes through (Arc 2 WP-A3, ARCH-01). Requires libsrt
//! loopback. The accept tests park a thread on purpose, so every wait is
//! bounded by [`WATCHDOG`] and FAILS on expiry instead of hanging.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tst_core::cancel::CancelSlot;
use tst_core::transport::{Transport, TransportError};
use tst_srt::{ListenerBuilder, SocketBuilder, SrtError, SrtUrl};

/// Upper bound on anything that must NOT park forever.
const WATCHDOG: Duration = Duration::from_secs(10);

/// Reserve an ephemeral UDP port and release it again, so a listener-mode
/// URL can name its port BEFORE the listener under test binds it (same
/// helper as `listener_accept_one_cancellable.rs`).
fn reserve_port() -> u16 {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);
    port
}

fn ipv6_loopback_available() -> bool {
    std::net::UdpSocket::bind("[::1]:0").is_ok()
}

/// Caller mode: the overlay is applied, the sender defaults are merged,
/// the transport is connected and alive, bytes arrive at the peer.
#[test]
fn connect_round_trips_over_loopback() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?latency=120&x-sendtimeout=5000"
    ))
    .expect("parse");
    let mut t = url.connect().expect("SrtUrl::connect");
    assert!(t.is_alive(), "a freshly connected transport is alive");
    t.send_bytes(b"hello via SrtUrl::connect")
        .expect("send_bytes");

    assert_eq!(accept.join(), b"hello via SrtUrl::connect");
    t.close();
}

/// The #188 class, end-to-end half: `parse` strips the brackets
/// (`host == "::1"`), so the open path must put them back — an IPv6
/// `srt://` URL has to connect and carry bytes, which is what PR #188
/// fixed in the bindings' private joins.
///
/// The bracketing *itself* is pinned by
/// `tst_srt::addr::tests::join_host_port_brackets_bare_ipv6_only`, not
/// here: mutating `join_host_port` to a plain `format!("{host}:{port}")`
/// leaves this test GREEN, because `ToSocketAddrs for str` splits at the
/// LAST `':'` and hands the rest to `getaddrinfo`, so `::1:PORT` still
/// resolves (measured on glibc). The unbracketed join is still wrong: the
/// v6 wildcard `::` joins to `::PORT`, whose split leaves the host `":"`
/// and fails to resolve, and the form is out of contract everywhere.
#[test]
fn connect_ipv6_literal_round_trips() {
    if !ipv6_loopback_available() {
        eprintln!("SKIP: IPv6 loopback unavailable on this host");
        return;
    }
    let mut builder = ListenerBuilder::new();
    builder.recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_at(builder, "[::1]:0");
    let port = lb.port;
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://[::1]:{port}")).expect("parse");
    assert_eq!(url.host, "::1", "parse hands the host back bracket-less");
    let mut t = url
        .connect()
        .expect("v6 caller connect through SrtUrl::connect (the #188 bracket class)");
    t.send_bytes(b"hello over v6").expect("send_bytes");

    assert_eq!(accept.join(), b"hello over v6");
    t.close();
}
