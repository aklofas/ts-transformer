//! Verifies connect/bind walk past failing resolved addresses.

use std::net::ToSocketAddrs;
use std::time::Duration;
use tst_srt::SocketBuilder;

#[test]
fn connect_walks_to_v4_when_v6_first_and_unbindable() {
    require_loopback!();
    // Bind a listener on 127.0.0.1 only. If we walk the addr iterator,
    // we should reach the v4 address even if ::1 comes first when
    // resolving "localhost".
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    // Sanity: confirm "localhost:<port>" resolves to multiple addresses.
    let addrs: Vec<_> = format!("localhost:{port}")
        .to_socket_addrs()
        .expect("resolve")
        .collect();
    if addrs.len() < 2 {
        eprintln!(
            "skipping: localhost resolved to only {} address(es)",
            addrs.len()
        );
        return;
    }

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .connect_timeout(Duration::from_secs(2))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("localhost:{port}"))
        .expect("connect should walk past unbindable ::1 to bindable 127.0.0.1");

    let _ = accept.join();
    drop(socket);
}
