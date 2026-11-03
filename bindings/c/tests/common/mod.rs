//! Helpers shared by every tst-c integration-test binary. Each binary root
//! declares `#[allow(dead_code)] mod common;` because per-file feature
//! gates decide which helpers a given build actually calls.

use std::time::Duration;

use tstrans::error::tst_get_last_error_str;

/// Read the C ABI's thread-local last-error string as an owned `String`;
/// `"<null>"` when the slot is empty, so a failing assertion still prints
/// something useful.
pub(crate) fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// One 188-byte null TS packet (PID 0x1FFF), the smallest valid payload for
/// the sender tests.
pub(crate) fn ts_null_packet() -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
}

/// `base` stretched by the `TST_TEST_TIMEOUT_SCALE` environment variable
/// (a whole number, default 1). The sanitizer jobs set it because they run
/// the peer threads several times slower. Unset, the margin is unchanged.
pub(crate) fn scaled(base: Duration) -> Duration {
    let scale = std::env::var("TST_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1);
    base * scale
}
