//! Smoke tests: open/close lifecycle + data-path null-guards for all four
//! UDP handle families.
//!
//! Gated on `feature = "udp"` so they compile only in builds that include
//! the UDP transport.
//!
//! The lifecycle tests verify that each of the four handle types opens
//! cleanly on a loopback / zero-port URL and closes without UB (no
//! double-free, no leak detectable by Miri/Valgrind). A malformed URL
//! must return null with the `UdpConfig` (-27) last-error code. The
//! data-path tests verify the entry points reject null arguments with
//! the correct error codes — they do not require an active peer.
//!
//! Note: the lib name for `tst-c` is `tstrans` (see `[lib] name` in
//! Cargo.toml); integration tests reference it as `tstrans`, not `tst_c`.
//!
//! There is no `tst_udp_*_cancel` entry point yet (new symbols, ABI 0.22),
//! so these tests do not exercise a cancel path — receivers open on an
//! ephemeral port and close without a blocking recv. The cross-thread
//! close that the UDP transport's real cancel handle now unblocks is
//! covered by `udp_close_cancels_first.rs`.
#![cfg(feature = "udp")]

use std::ffi::CString;

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::event::TstEvent;
use tstrans::stats::{
    TST_CODEC_KIND_UNKNOWN, TstDemuxReceiverStats, TstMuxSenderStats, TstReceiverStats,
    TstSenderStats, TstSocketStats, TstStreamCodecStats, TstStreamCodecStatsUnion,
};
use tstrans::udp::{
    tst_udp_demux_receiver_close, tst_udp_demux_receiver_get_socket_stats,
    tst_udp_demux_receiver_get_stats, tst_udp_demux_receiver_get_stream_codec_stats,
    tst_udp_demux_receiver_get_stream_stats, tst_udp_demux_receiver_next_event,
    tst_udp_demux_receiver_open, tst_udp_demux_receiver_reset_stats, tst_udp_mux_sender_close,
    tst_udp_mux_sender_get_mux_sender_stats, tst_udp_mux_sender_get_socket_stats,
    tst_udp_mux_sender_open, tst_udp_mux_sender_reset_stats, tst_udp_receiver_close,
    tst_udp_receiver_get_socket_stats, tst_udp_receiver_get_stats, tst_udp_receiver_recv_ts,
    tst_udp_receiver_reset_stats, tst_udp_recv_open, tst_udp_sender_close,
    tst_udp_sender_get_socket_stats, tst_udp_sender_get_stats, tst_udp_sender_open,
    tst_udp_sender_reset_stats, tst_udp_sender_send_ts,
};

// ---------------------------------------------------------------------------
// Lifecycle smoke (open + close) — one per handle family
// ---------------------------------------------------------------------------

/// Open a UDP sender to a loopback unicast address. UDP is connectionless
/// so the open succeeds without a peer. Handle must be non-null and close
/// without crashing.
#[test]
fn udp_sender_open_unicast_returns_handle() {
    let url = CString::new("udp://127.0.0.1:54401").unwrap();
    let handle = unsafe { tst_udp_sender_open(url.as_ptr()) };
    assert!(
        !handle.is_null(),
        "tst_udp_sender_open returned null for udp://127.0.0.1:54401"
    );
    unsafe { tst_udp_sender_close(handle) };
}

/// Bind a receiver on the loopback with port 0 (kernel-assigned).
#[test]
fn udp_recv_open_unicast_zero_port_returns_handle() {
    let url = CString::new("udp://127.0.0.1:0").unwrap();
    let handle = unsafe { tst_udp_recv_open(url.as_ptr()) };
    assert!(
        !handle.is_null(),
        "tst_udp_recv_open returned null for udp://127.0.0.1:0"
    );
    unsafe { tst_udp_receiver_close(handle) };
}

/// Open a UDP-backed mux sender on loopback with a minimal one-video-stream
/// config. Handle must be non-null and close without crashing.
#[test]
fn udp_mux_sender_open_returns_handle() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("udp://127.0.0.1:54402").unwrap();
    let handle = unsafe { tst_udp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!handle.is_null(), "tst_udp_mux_sender_open returned null");
    unsafe { tst_udp_mux_sender_close(handle) };
}

/// Open a UDP-backed demux receiver on a kernel-assigned port with default
/// demux config (NULL).
#[test]
fn udp_demux_receiver_open_returns_handle() {
    let url = CString::new("udp://127.0.0.1:0").unwrap();
    let handle = unsafe { tst_udp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(
        !handle.is_null(),
        "tst_udp_demux_receiver_open returned null"
    );
    unsafe { tst_udp_demux_receiver_close(handle) };
}

// ---------------------------------------------------------------------------
// Malformed URL → null + UdpConfig (-27)
// ---------------------------------------------------------------------------

/// A malformed URL must return null and set last-error == UdpConfig (-27).
/// The parse-error path runs before the (stubbed) udp_error_to_code mapper,
/// so this code is deterministic regardless of the build-error mapping.
#[test]
fn udp_sender_open_malformed_url_returns_null_with_udp_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_udp_sender_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_udp_sender_open should return null for a malformed URL"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::UdpConfig as i32,
        "malformed-url parse failure should set UdpConfig (-27), got {code}"
    );
}

#[test]
fn udp_recv_open_malformed_url_returns_null_with_udp_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_udp_recv_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_udp_recv_open should return null for a malformed URL"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(code, TstError::UdpConfig as i32, "got {code}");
}

#[test]
fn udp_demux_receiver_open_malformed_url_returns_null_with_udp_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_udp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(handle.is_null());
    let code = unsafe { tst_get_last_error() };
    assert_eq!(code, TstError::UdpConfig as i32, "got {code}");
}

// ---------------------------------------------------------------------------
// Multicast with ?iface=lo — lenient: accept null OR a negative error.
// ---------------------------------------------------------------------------

/// A multicast sender with `?iface=lo` may succeed (returning a handle) or
/// fail (returning null). If it fails, the last-error must be a negative
/// code — we accept any of UdpIo / UdpConfig / UdpIfaceUnsupported because
/// the udp_error_to_code mapper is stubbed (returns UdpIo today, refined to
/// UdpIfaceUnsupported in a later wave). We do NOT assert a specific code.
#[test]
fn udp_mux_sender_multicast_iface_lo_is_lenient() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("udp://239.10.0.1:54403?iface=lo").unwrap();
    let handle = unsafe { tst_udp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };

    if handle.is_null() {
        let code = unsafe { tst_get_last_error() };
        assert!(
            code == TstError::UdpIo as i32
                || code == TstError::UdpConfig as i32
                || code == TstError::UdpIfaceUnsupported as i32,
            "multicast iface=lo failure should be a UDP-family code, got {code}"
        );
    } else {
        // Some platforms accept iface=lo for multicast send — close cleanly.
        unsafe { tst_udp_mux_sender_close(handle) };
    }
}

/// Same leniency for a raw multicast receiver joining on `?iface=lo`. The
/// receiver uses the ffmpeg `@` prefix to mark a multicast bind.
#[test]
fn udp_recv_multicast_iface_lo_is_lenient() {
    let url = CString::new("udp://@239.10.0.2:54404?iface=lo").unwrap();
    let handle = unsafe { tst_udp_recv_open(url.as_ptr()) };

    if handle.is_null() {
        let code = unsafe { tst_get_last_error() };
        assert!(
            code == TstError::UdpIo as i32
                || code == TstError::UdpConfig as i32
                || code == TstError::UdpIfaceUnsupported as i32,
            "multicast iface=lo failure should be a UDP-family code, got {code}"
        );
    } else {
        unsafe { tst_udp_receiver_close(handle) };
    }
}

// ---------------------------------------------------------------------------
// Stats round-trips on live handles (no peer required)
// ---------------------------------------------------------------------------

#[test]
fn udp_sender_stats_and_reset() {
    let url = CString::new("udp://127.0.0.1:54405").unwrap();
    let h = unsafe { tst_udp_sender_open(url.as_ptr()) };
    assert!(!h.is_null());

    let mut stats = TstSenderStats::default();
    assert_eq!(unsafe { tst_udp_sender_get_stats(h, &mut stats) }, 0);
    assert_eq!(unsafe { tst_udp_sender_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_udp_sender_get_socket_stats(h, &mut ss) };

    unsafe { tst_udp_sender_close(h) };
}

#[test]
fn udp_receiver_stats_and_reset() {
    let url = CString::new("udp://127.0.0.1:0").unwrap();
    let h = unsafe { tst_udp_recv_open(url.as_ptr()) };
    assert!(!h.is_null());

    let mut stats = TstReceiverStats::default();
    assert_eq!(unsafe { tst_udp_receiver_get_stats(h, &mut stats) }, 0);
    assert_eq!(unsafe { tst_udp_receiver_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_udp_receiver_get_socket_stats(h, &mut ss) };

    unsafe { tst_udp_receiver_close(h) };
}

#[test]
fn udp_mux_sender_stats_and_reset() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("udp://127.0.0.1:54406").unwrap();
    let h = unsafe { tst_udp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!h.is_null());

    let mut stats = TstMuxSenderStats::default();
    assert_eq!(
        unsafe { tst_udp_mux_sender_get_mux_sender_stats(h, &mut stats) },
        0
    );
    assert_eq!(unsafe { tst_udp_mux_sender_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_udp_mux_sender_get_socket_stats(h, &mut ss) };

    unsafe { tst_udp_mux_sender_close(h) };
}

#[test]
fn udp_demux_receiver_stats_and_reset() {
    let url = CString::new("udp://127.0.0.1:0").unwrap();
    let h = unsafe { tst_udp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(!h.is_null());

    let mut stats = TstDemuxReceiverStats::default();
    assert_eq!(
        unsafe { tst_udp_demux_receiver_get_stats(h, &mut stats) },
        0
    );
    assert_eq!(unsafe { tst_udp_demux_receiver_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_udp_demux_receiver_get_socket_stats(h, &mut ss) };

    let mut cs = TstStreamCodecStats {
        kind: TST_CODEC_KIND_UNKNOWN,
        _pad: 0,
        u: TstStreamCodecStatsUnion {
            unknown: Default::default(),
        },
    };
    // No PIDs observed yet — expect NOT_FOUND.
    let rc = unsafe { tst_udp_demux_receiver_get_stream_codec_stats(h, 0x0100, &mut cs) };
    assert_eq!(rc, TstError::NotFound as i32);

    // Per-PID borrowed buffer — should return 0 and an empty slice.
    let mut arr: *const tstrans::stats::TstStreamStats = std::ptr::null();
    let mut count: libc::size_t = 0;
    let rc = unsafe { tst_udp_demux_receiver_get_stream_stats(h, &mut arr, &mut count) };
    assert_eq!(rc, 0);
    assert_eq!(count, 0);

    unsafe { tst_udp_demux_receiver_close(h) };
}

// ---------------------------------------------------------------------------
// Null-pointer guards (data-path must not crash/panic on null)
// ---------------------------------------------------------------------------

#[test]
fn null_send_ts_returns_invalid_config() {
    let rc = unsafe { tst_udp_sender_send_ts(std::ptr::null_mut(), std::ptr::null(), 0) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn null_recv_ts_returns_invalid_config() {
    let mut buf = [0u8; 188];
    let mut n: usize = 0;
    let rc = unsafe {
        tst_udp_receiver_recv_ts(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut n)
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn null_next_event_returns_invalid_config() {
    let mut ev = TstEvent::default();
    let rc = unsafe { tst_udp_demux_receiver_next_event(std::ptr::null_mut(), &mut ev) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}
