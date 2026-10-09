//! Live-socket verification that `MaxBandwidth::InputRelative` reaches libsrt as 0.
//! Regression for the `MaxBandwidth::Infinite = -2` bug.
//!
//! libsrt rejects any `SRTO_MAXBW < -1` with `MJ_NOTSUP / MN_INVAL` (see
//! `socketconfig.cpp`), so the previous `Infinite` variant always errored at
//! runtime. This test pins the only correct way to express "no cap" — the
//! `InputRelative` variant, which maps to libsrt's sentinel `0` — through a real
//! handshake against a local listener.

use std::time::Duration;
use tst_srt::{MaxBandwidth, SocketBuilder};

#[test]
fn input_relative_max_bandwidth_reaches_libsrt_as_zero() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        drop(sock);
    });
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .max_bandwidth(MaxBandwidth::InputRelative)
        .recv_timeout(Duration::from_secs(5))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    // Join the accept thread BEFORE dropping the connecting socket. Dropping it
    // first tears the connection down while the listener is still inside
    // accept(); under the consolidated loopback binary's parallelism the accept
    // thread loses the CPU race and accept() returns "Connection was broken".
    // Sibling tests (e.g. getaddrinfo_walk) already join-then-drop for this reason.
    accept.join();
    drop(socket);
}
