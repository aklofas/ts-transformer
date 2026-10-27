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
//! Cancel entry points (`tst_rist_*_cancel`, ABI 0.22) get their
//! null-guard / idempotence coverage below. The cross-thread behaviour
//! needs its own shape — a `tst_rist_*_recv_ts` never parks (one ~100 ms
//! librist poll, `TST_E_BUFFER_FULL` when empty), so a C caller polls in a
//! loop and the pin is "a cancel from another thread ends the LOOP with
//! `TST_E_CLOSED`": that lives in `rist_cancel_from_other_thread.rs`.
//!
//! RIST receiver URLs require the `@` bind prefix (ffmpeg convention):
//! `rist://@127.0.0.1:0`. Sender URLs use no prefix: `rist://host:port`.
#![cfg(feature = "rist")]

use std::ffi::CString;

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::event::TstEvent;
use tstrans::rist::{
    tst_rist_demux_receiver_cancel, tst_rist_demux_receiver_close,
    tst_rist_demux_receiver_next_event, tst_rist_demux_receiver_open, tst_rist_mux_sender_cancel,
    tst_rist_mux_sender_close, tst_rist_mux_sender_finish, tst_rist_mux_sender_open,
    tst_rist_receiver_cancel, tst_rist_receiver_close, tst_rist_recv_open, tst_rist_sender_cancel,
    tst_rist_sender_close, tst_rist_sender_open,
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

/// 9968 bytes is the largest payload librist carries in one datagram, so a
/// sender URL asking for 10 KiB is a configuration error, not a send-time one.
#[test]
fn pkt_size_10k_is_rist_invalid_config() {
    let handle = unsafe { tst_rist_sender_open(c"rist://127.0.0.1:19022?pkt_size=10K".as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_rist_sender_open should refuse pkt_size=10K"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::RistConfig as i32,
        "pkt_size above 9968 should set RistConfig (-39), got {code}"
    );
}

// ---------------------------------------------------------------------------
// Encryption: the outcome follows the `mbedtls` feature exactly
// ---------------------------------------------------------------------------

/// `mbedtls` on (the default with `rist`): an AES-256 receiver opens.
#[cfg(feature = "mbedtls")]
#[test]
fn rist_recv_open_aes256_opens_when_mbedtls_is_on() {
    let url = CString::new("rist://@127.0.0.1:0?aes-type=256&secret=test-key&buffer=200").unwrap();
    let handle = unsafe { tst_rist_recv_open(url.as_ptr()) };
    assert!(
        !handle.is_null(),
        "encrypted open refused: {} ({})",
        unsafe { tst_get_last_error() },
        crate::common::last_error_msg()
    );
    unsafe { tst_rist_receiver_close(handle) };
}

/// `mbedtls` off: the same URL is refused with RistEncryptionDisabled (-41).
#[cfg(not(feature = "mbedtls"))]
#[test]
fn rist_recv_open_aes256_is_refused_without_mbedtls() {
    let url = CString::new("rist://@127.0.0.1:0?aes-type=256&secret=test-key&buffer=200").unwrap();
    let handle = unsafe { tst_rist_recv_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "encrypted open must be refused without mbedtls"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::RistEncryptionDisabled as i32,
        "expected RistEncryptionDisabled (-41), got {code}"
    );
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

// ---------------------------------------------------------------------------
// `_finish` — ABI 0.22
// ---------------------------------------------------------------------------

/// `tst_rist_mux_sender_finish` drains and closes.
///
/// Nothing was ever pushed, so `pending_bytes` is empty and this takes the
/// EMPTY-DRAIN path: `finish` has nothing to send and closes. The rc is
/// asserted leniently only because no receiver is listening, so librist's
/// own teardown may report an error; what must hold is that the call
/// neither panics nor mis-reports a null pointer, that a second call is 0,
/// and that the handle still frees with `_close`. The drain-ERROR arm is
/// pinned once for the whole PR by the tcp shell's
/// `tcp_mux_sender_finish_reports_the_drain_error`.
///
/// Port 33106 is EVEN (the Simple profile puts RTCP on `port + 1`) and
/// disjoint from every other reserved range: `loopback.rs` 33010–33026,
/// `cancel.rs` 33040–33048, `conformance.rs` 33050–33098, the
/// cross-thread pin 33100, the null-guard test 33104 and the pytest
/// suite 34110–34150.
#[test]
fn rist_mux_sender_finish_then_close() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("rist://127.0.0.1:33106").unwrap();
    let h = unsafe { tst_rist_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!h.is_null(), "tst_rist_mux_sender_open returned null");

    let rc = unsafe { tst_rist_mux_sender_finish(h) };
    assert!(
        rc == 0
            || (rc < 0
                && rc != TstError::PanicCaught as i32
                && rc != TstError::InvalidConfig as i32),
        "finish rc={rc}"
    );
    assert_eq!(
        unsafe { tst_rist_mux_sender_finish(h) },
        0,
        "second finish is 0"
    );
    assert_eq!(
        unsafe { tst_rist_mux_sender_finish(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );

    unsafe { tst_rist_mux_sender_close(h) };
}

// ---------------------------------------------------------------------------
// `_cancel` — ABI 0.22
// ---------------------------------------------------------------------------

/// Every `tst_rist_*_cancel` returns 0 on a live handle and
/// `TST_E_INVALID_CONFIG` on NULL, and `_cancel` never consumes the handle
/// (the `_close` after it still frees).
///
/// The cross-thread behaviour — a cancel ending a C-side RIST poll loop
/// with `TST_E_CLOSED` — lives in `rist_cancel_from_other_thread.rs`, which
/// owns the reserved port 33100. This test uses 33104 (EVEN: the Simple
/// profile puts RTCP on `port + 1`), disjoint from that, from
/// `rist_mux_sender_finish_then_close`'s 33106, and from every Rust-side
/// range (`loopback.rs` 33010–33026, `cancel.rs` 33040–33048,
/// `conformance.rs` 33050–33098, pytest 34110–34150).
#[test]
fn rist_cancel_entry_points_return_ok_and_null_is_invalid_config() {
    let url = CString::new("rist://127.0.0.1:33104").unwrap();
    let s = unsafe { tst_rist_sender_open(url.as_ptr()) };
    assert!(!s.is_null(), "tst_rist_sender_open failed: {}", unsafe {
        tst_get_last_error()
    });
    assert_eq!(unsafe { tst_rist_sender_cancel(s) }, 0);
    assert_eq!(unsafe { tst_rist_sender_cancel(s) }, 0, "idempotent");
    unsafe { tst_rist_sender_close(s) };

    assert_eq!(
        unsafe { tst_rist_sender_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_rist_mux_sender_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_rist_receiver_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_rist_demux_receiver_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
}
