//! Verifies SRTO_LINGER setter reaches libsrt and short-linger lets Drop
//! return promptly.

use std::time::{Duration, Instant};
use tst_srt::SocketBuilder;

#[test]
fn drop_with_zero_linger_does_not_block() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        // Just hold the accepted socket until the main thread drops the
        // caller — that's the path linger=0 needs to short-circuit.
        drop(sock);
    });
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .linger(Duration::from_secs(0))
        .send_timeout(Duration::from_secs(5))
        .connect(("127.0.0.1", port))
        .expect("connect");

    accept.join();

    let drop_started = Instant::now();
    drop(socket);
    let drop_elapsed = drop_started.elapsed();

    assert!(
        drop_elapsed < Duration::from_secs(2),
        "Drop with linger=0s should return promptly, took {drop_elapsed:?}",
    );
}
