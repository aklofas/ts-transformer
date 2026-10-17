//! Stats post-IO sanity check.

use std::sync::mpsc;
use std::time::Duration;
use tst_srt::SrtSocketBuilder;

#[test]
fn stats_after_round_trip_show_nonzero_bytes() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    // Channel: listener sends (n, bytes_received) after recv + stats, then
    // waits for the main thread to release it so the socket stays alive long
    // enough for the caller-side stats call.
    let (tx, rx) = mpsc::channel::<(usize, u64)>();
    let (done_tx, done_rx) = mpsc::channel::<()>();

    let accept = lb.spawn_accept(move |mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        let stats = sock.stats().expect("listener stats");
        tx.send((n, stats.bytes_received)).unwrap();
        // Keep the socket alive until the main thread has called its stats.
        done_rx.recv().unwrap();
    });
    accept.wait_ready();

    let mut socket = SrtSocketBuilder::new()
        .recv_timeout(Duration::from_secs(5))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    socket.send(&vec![0u8; 1000]).expect("send");

    // Wait for listener to confirm receipt before querying caller stats.
    let (n, bytes_recv) = rx.recv().expect("listener result");
    assert_eq!(n, 1000);
    assert!(
        bytes_recv >= 1000,
        "stats.bytes_received={bytes_recv} < 1000"
    );

    // Caller stats: connection still live because listener is blocked on done_rx.
    let caller_stats = socket.stats().expect("caller stats");
    assert!(
        caller_stats.bytes_sent >= 1000,
        "stats.bytes_sent={} < 1000",
        caller_stats.bytes_sent
    );

    done_tx.send(()).unwrap();
    accept.join();
}
