//! Test fixtures shared across `tst-rtp` integration tests.

#![allow(dead_code)] // not every test uses every helper

use std::time::{Duration, Instant};

pub mod h264_payloader;
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
