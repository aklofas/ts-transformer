//! Stream ID negotiation: caller-set ID is visible on the accepted Socket.

use std::time::Duration;
use tst_srt::{SrtSocketBuilder, StreamId};

#[test]
fn stream_id_round_trips() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| sock.stream_id().map(|s| s.to_string()));
    accept.wait_ready();

    let _socket = SrtSocketBuilder::new()
        .stream_id(StreamId::new("publish:cam1").unwrap())
        .recv_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    let observed = accept.join();
    assert_eq!(observed.as_deref(), Some("publish:cam1"));
}
