//! IPv6 loopback round-trip test for plan #29 Task 5.2 (audit Critical #11).
//!
//! Spawns a Listener bound on `[::1]:0`, connects a Socket to the
//! discovered ephemeral port, sends a payload, recv'd. Confirms the new
//! v4+v6 dispatch in tst_srt::addr (Task 5.1, commit 5c577d8) works
//! against a real libsrt socket.

use std::net::IpAddr;
use std::time::Duration;
use tst_srt::{ListenerBuilder, SocketBuilder};

#[test]
fn ipv6_loopback_round_trip() {
    if !ipv6_loopback_available() {
        eprintln!("skipping: IPv6 loopback unavailable on this host");
        return;
    }

    // The shared fixture (rather than a hand-rolled accept thread) so the
    // peer's `accept()` cannot park forever — see `AcceptHandle`'s docs.
    let mut builder = ListenerBuilder::new();
    builder.recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_at(builder, "[::1]:0");
    let local = lb.listener.local_addr().expect("local_addr");
    assert!(
        matches!(local.ip(), IpAddr::V6(_)),
        "listener bound on v6, got {local:?}"
    );
    let port = lb.port;

    let accept = lb.spawn_accept(|mut socket| {
        let mut buf = [0u8; 1316];
        let n = socket.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let mut sender = SocketBuilder::new()
        .recv_timeout(Duration::from_secs(5))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("[::1]:{port}"))
        .expect("v6 caller connect");
    sender.send(b"hello over v6").expect("send");

    let received = accept.join();
    assert_eq!(received, b"hello over v6");
}

fn ipv6_loopback_available() -> bool {
    // Some CI environments disable IPv6 loopback (`ip6_disabled`,
    // unprivileged containers without v6 stack). Probe by trying to
    // bind a UDP socket; if that fails, skip the test.
    std::net::UdpSocket::bind("[::1]:0").is_ok()
}
