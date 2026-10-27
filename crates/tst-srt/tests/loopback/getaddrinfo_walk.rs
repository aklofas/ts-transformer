//! Verifies connect walks past an address that never answers.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;
use tst_srt::SrtSocketBuilder;

#[test]
fn connect_walks_to_v4_when_v6_first_and_unbindable() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let live: SocketAddr = ([127, 0, 0, 1], lb.port).into();

    // A bound but silent UDP socket: the handshake to it gets no reply, so
    // the walk must time out on it and move to the live address.
    let silent = UdpSocket::bind("127.0.0.1:0").expect("bind silent socket");
    let dead = silent.local_addr().expect("silent addr");

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SrtSocketBuilder::new()
        .connect_timeout(Duration::from_millis(500))
        .send_timeout(Duration::from_secs(5))
        .connect(&[dead, live][..])
        .expect("connect should walk past the silent address to the live one");

    assert_eq!(socket.peer_addr().unwrap(), live);
    let _ = accept.join();
    drop(socket);
    drop(silent);
}
