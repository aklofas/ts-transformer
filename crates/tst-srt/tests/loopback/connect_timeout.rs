//! Verifies SRTO_CONNTIMEO is honored.

use std::time::{Duration, Instant};
use tst_srt::SocketBuilder;

#[test]
fn connect_timeout_fires_after_configured_duration() {
    // 198.51.100.0/24 (TEST-NET-2) is reserved per RFC 5737 and must not be routed.
    // Connecting there will hang until the SRT connect timeout fires.
    let started = Instant::now();
    let result = SocketBuilder::new()
        .connect_timeout(Duration::from_millis(800))
        .connect("198.51.100.1:9000");
    let elapsed = started.elapsed();

    assert!(result.is_err(), "expected connect to fail to TEST-NET-2");
    assert!(
        elapsed >= Duration::from_millis(700) && elapsed < Duration::from_secs(3),
        "connect_timeout 800ms should fire well before libsrt's 3s default; \
         elapsed={elapsed:?}",
    );
}
