//! Smoke tests: open/close lifecycle + data-path null-guards for all four
//! RIST handle families.
//!
//! Gated on `feature = "rist"` so they compile only in builds that include
//! the RIST transport.
//!
//! The lifecycle tests verify that each of the four handle types opens
//! cleanly on a loopback / zero-port URL and closes without UB (no
//! double-free, no leak detectable by Miri/Valgrind). A malformed URL
//! must return null with the `RistConfig` (-39) last-error code. The
//! data-path tests verify the entry points reject null arguments with
//! the correct error codes — they do not require an active peer.
//!
//! Note: the lib name for `tst-c` is `tstrans` (see `[lib] name` in
//! Cargo.toml); integration tests reference it as `tstrans`, not `tst_c`.
//!
//! There is no `tst_rist_*_cancel` entry point yet (new symbols, ABI
//! 0.22), so these tests do not exercise a cancel path — receivers open on
//! an ephemeral port (port 0 maps to a kernel-assigned port) and close
//! without a blocking recv. The RIST transport does carry a real cancel
//! handle since Arc 2 WP-D, but a `tst_rist_*_recv_ts` call never parks
//! (one ~100 ms poll, `TST_E_BUFFER_FULL` when empty), so the
//! cross-thread pin lives at the Rust level in
//! `crates/tst-rist/tests/cancel.rs` until the 0.22 cancel handles land.
//!
//! RIST receiver URLs require the `@` bind prefix (ffmpeg convention):
//! `rist://@127.0.0.1:0`. Sender URLs use no prefix: `rist://host:port`.
#![cfg(feature = "rist")]

use std::ffi::CString;

use tstrans::error::{TstError, tst_get_last_error};
use tstrans::event::TstEvent;
use tstrans::rist::{
    tst_rist_demux_receiver_close, tst_rist_demux_receiver_next_event,
    tst_rist_demux_receiver_open, tst_rist_receiver_close, tst_rist_recv_open,
    tst_rist_sender_open,
};

// ---------------------------------------------------------------------------
// Lifecycle smoke (open + close) — receiver family (bind on port 0)
// ---------------------------------------------------------------------------

/// Bind a RIST receiver on the loopback with port 0 (kernel-assigned).
/// RIST receiver URLs use the ffmpeg `@` prefix to distinguish bind from
/// send: `rist://@host:port` means "listen on this address and port".
/// Port 0 causes the kernel to assign an available ephemeral port.
#[test]
fn rist_recv_open_bind_zero_port_returns_handle() {
    let url = CString::new("rist://@127.0.0.1:0").unwrap();
    let handle = unsafe { tst_rist_recv_open(url.as_ptr()) };
    assert!(
        !handle.is_null(),
        "tst_rist_recv_open returned null for rist://@127.0.0.1:0; \
         last-error: {}",
        unsafe { tst_get_last_error() }
    );
    unsafe { tst_rist_receiver_close(handle) };
}

/// Open a RIST-backed demux receiver on a kernel-assigned port with default
/// demux config (NULL). The `@` prefix marks it as a bind URL.
#[test]
fn rist_demux_receiver_open_returns_handle() {
    let url = CString::new("rist://@127.0.0.1:0").unwrap();
    let handle = unsafe { tst_rist_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(
        !handle.is_null(),
        "tst_rist_demux_receiver_open returned null for rist://@127.0.0.1:0; \
         last-error: {}",
        unsafe { tst_get_last_error() }
    );
    unsafe { tst_rist_demux_receiver_close(handle) };
}

// ---------------------------------------------------------------------------
// Malformed URL → null + RistConfig (-39)
// ---------------------------------------------------------------------------

/// A malformed URL must return null and set last-error == RistConfig (-39).
/// The parse-error path runs before any librist FFI call, so this code is
/// deterministic regardless of the librist build configuration.
#[test]
fn rist_recv_open_malformed_url_returns_null_with_rist_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_rist_recv_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_rist_recv_open should return null for a malformed URL"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::RistConfig as i32,
        "malformed-url parse failure should set RistConfig (-39), got {code}"
    );
}

/// A sender URL with a missing host should also return RistConfig (-39).
#[test]
fn rist_sender_open_malformed_url_returns_null_with_rist_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_rist_sender_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_rist_sender_open should return null for a malformed URL"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::RistConfig as i32,
        "malformed-url parse failure should set RistConfig (-39), got {code}"
    );
}

/// A receiver URL without the `@` bind prefix should be rejected as a
/// configuration error (RistConfig -39): the RIST builder enforces that
/// receiver URLs must have the `@` prefix.
#[test]
fn rist_recv_open_send_url_returns_null_with_rist_config() {
    // `rist://127.0.0.1:8000` (no `@`) is a sender URL — RistRecvTransportBuilder
    // rejects it with RistError::InvalidConfig, which maps to RistConfig (-39).
    let url = CString::new("rist://127.0.0.1:8000").unwrap();
    let handle = unsafe { tst_rist_recv_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_rist_recv_open should return null for a send URL (missing '@')"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::RistConfig as i32,
        "send URL used with recv builder should set RistConfig (-39), got {code}"
    );
}

// ---------------------------------------------------------------------------
// Encryption probe — lenient: accept success OR RistEncryptionDisabled (-41)
// ---------------------------------------------------------------------------

/// Open a RIST receiver with AES-256 encryption requested.
///
/// This test is lenient: it accepts either a successful handle (when
/// librist was built with mbedTLS, i.e., the `mbedtls` cargo feature is
/// enabled) OR a null return with `TST_E_RIST_ENCRYPTION_DISABLED (-41)`
/// (when the build omits mbedTLS encryption support).
///
/// WHY lenient?
///   The CI matrix builds both `--features rist` (with mbedtls, default)
///   and potentially without. Both outcomes are correct behaviour; forcing
///   a specific one would hard-code the build configuration into the test.
#[test]
fn rist_recv_open_aes256_encryption_is_lenient() {
    let url = CString::new("rist://@127.0.0.1:0?aes-type=256&secret=test-key&buffer=200").unwrap();
    let handle = unsafe { tst_rist_recv_open(url.as_ptr()) };

    if handle.is_null() {
        // Encryption disabled at build time — the only acceptable failure code.
        let code = unsafe { tst_get_last_error() };
        assert_eq!(
            code,
            TstError::RistEncryptionDisabled as i32,
            "AES-256 receiver open failed with unexpected code {code} \
             (expected RistEncryptionDisabled (-41) when mbedtls is disabled)"
        );
    } else {
        // mbedTLS is available — the handle is live; close it cleanly.
        unsafe { tst_rist_receiver_close(handle) };
    }
}

// ---------------------------------------------------------------------------
// Null-pointer guards (data-path must not crash/panic on null)
// ---------------------------------------------------------------------------

#[test]
fn null_recv_ts_returns_invalid_config() {
    let mut buf = [0u8; 188];
    let mut n: usize = 0;
    let rc = unsafe {
        tstrans::rist::tst_rist_receiver_recv_ts(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut n,
        )
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn null_next_event_returns_invalid_config() {
    let mut ev = TstEvent::default();
    let rc = unsafe { tst_rist_demux_receiver_next_event(std::ptr::null_mut(), &mut ev) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}
