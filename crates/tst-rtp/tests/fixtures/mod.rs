//! Test fixtures shared across `tst-rtp` integration tests.

#![allow(dead_code)] // not every test uses every helper

use std::time::{Duration, Instant};

pub mod rtp_packets;
pub mod raw_rtsp;
pub mod raw_rtsp_publisher;
pub mod raw_rtsp_server;
pub mod rtsp_loopback_server;

#[cfg(feature = "tls")]
pub mod tls_certs;

/// Poll `f` every 20 ms until it returns `Some`, or panic after `budget`.
/// For tests that race a background thread (a keepalive pinger, an
/// interleaved pump, a server reaping a dropped connection) against their
/// assertion, so a regression fails fast instead of hanging a CI runner.
pub fn wait_for<T>(budget: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() >= deadline {
            panic!("condition not met within {budget:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Call `push` (one batch of frames for a peer that stopped reading) until
/// a fanout is parked on that peer: one TCP-interleaved frame write holds
/// its session's writer lock and no frame write has finished for 300 ms
/// (see `RtspServer::interleaved_write_probe`). Pushing until then, not a
/// fixed amount first, fills the socket even when the fanout was starved
/// while the frames were produced. A running fanout finishes a frame every
/// few microseconds and a parked one never does again, so the wait cannot
/// pass early on a slow runner, only take longer. Panics after `budget`.
#[cfg(feature = "rtsp-server")]
pub fn push_until_fanout_parks(
    server: &tst_rtp::RtspServer,
    budget: Duration,
    mut push: impl FnMut(),
) {
    let quiet = Duration::from_millis(300);
    let mut last = server.interleaved_write_probe();
    let mut since = Instant::now();
    wait_for(budget, || {
        push();
        let now = server.interleaved_write_probe();
        if now.0 == 0 || now.1 != last.1 {
            last = now;
            since = Instant::now();
            return None;
        }
        (since.elapsed() >= quiet).then_some(())
    });
}
