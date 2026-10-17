//! Send/recv round-trip; timeouts; buffer-too-small.

use std::time::Duration;
use tst_srt::SrtSocketBuilder;
use tst_srt::error::RecvError;

#[test]
fn small_payload_round_trips() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let payload: &[u8] = b"hello, ts-transformer";
    let expected = payload.to_vec();

    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let mut socket = SrtSocketBuilder::new()
        .recv_timeout(Duration::from_secs(5))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    socket.send(payload).expect("send");

    let received = accept.join();
    assert_eq!(received, expected);
}

#[test]
fn recv_timeout_trips_typed_error() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        // Accept but never send — caller should hit recv timeout.
        std::thread::sleep(Duration::from_secs(2));
        drop(sock);
    });
    accept.wait_ready();

    let mut socket = SrtSocketBuilder::new()
        .recv_timeout(Duration::from_millis(500))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    let mut buf = [0u8; 1500];
    let result = socket.recv(&mut buf);
    match result {
        Err(RecvError::TimedOut) => { /* expected */ }
        other => panic!("expected RecvError::TimedOut; got {other:?}"),
    }

    accept.join();
}
