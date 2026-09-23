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
//! Cancel entry points (`tst_udp_*_cancel`, ABI 0.22) are exercised below:
//! a cancel from another thread ends a parked `_recv_ts` with
//! `TST_E_CLOSED` without freeing the handle. The freeing cross-thread
//! `_close` is covered separately by `udp_close_cancels_first.rs`.
#![cfg(feature = "udp")]

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
    tst_udp_demux_receiver_cancel, tst_udp_demux_receiver_close,
    tst_udp_demux_receiver_get_socket_stats, tst_udp_demux_receiver_get_stats,
    tst_udp_demux_receiver_get_stream_codec_stats, tst_udp_demux_receiver_get_stream_stats,
    tst_udp_demux_receiver_next_event, tst_udp_demux_receiver_open,
    tst_udp_demux_receiver_reset_stats, tst_udp_mux_sender_cancel, tst_udp_mux_sender_close,
    tst_udp_mux_sender_finish, tst_udp_mux_sender_get_mux_sender_stats,
    tst_udp_mux_sender_get_socket_stats, tst_udp_mux_sender_open, tst_udp_mux_sender_reset_stats,
    tst_udp_receiver_cancel, tst_udp_receiver_close, tst_udp_receiver_get_socket_stats,
    tst_udp_receiver_get_stats, tst_udp_receiver_recv_ts, tst_udp_receiver_reset_stats,
    tst_udp_recv_open, tst_udp_sender_cancel, tst_udp_sender_close,
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

// ---------------------------------------------------------------------------
// `_finish` — Arc 2 R3 (DEBT-14 "ship now" cell), ABI 0.22
// ---------------------------------------------------------------------------

/// `tst_udp_mux_sender_finish` drains and closes. UDP is connectionless, so
/// there is no peer to lose and the drain must report 0 exactly; a second
/// call is 0; a null pointer is `TST_E_INVALID_CONFIG`; the handle still
/// frees with `_close`.
#[test]
fn udp_mux_sender_finish_then_close() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("udp://127.0.0.1:54407").unwrap();
    let h = unsafe { tst_udp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!h.is_null(), "tst_udp_mux_sender_open returned null");

    let rc = unsafe { tst_udp_mux_sender_finish(h) };
    assert_eq!(rc, 0, "finish rc={rc}");
    assert_eq!(
        unsafe { tst_udp_mux_sender_finish(h) },
        0,
        "second finish is 0"
    );
    assert_eq!(
        unsafe { tst_udp_mux_sender_finish(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );

    unsafe { tst_udp_mux_sender_close(h) };
}

// ---------------------------------------------------------------------------
// `_cancel` — Arc 2 R4, ABI 0.22
// ---------------------------------------------------------------------------

/// `_cancel` is documented callable from any thread; the raw pointer just
/// needs to cross the thread boundary to get there.
struct SendUdpRx(*mut tstrans::udp::TstUdpReceiver);
unsafe impl Send for SendUdpRx {}

/// Ask the kernel for a free loopback UDP port by binding and releasing.
/// The C receiver handle exposes no local-port getter, so a test that needs
/// to know its port must pick it first. (Same helper shape as
/// `udp_close_cancels_first.rs::free_port`.)
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Arc 2 R4: `tst_udp_receiver_cancel` from another thread wakes a parked
/// `recv_ts` with `TST_E_CLOSED`.
///
/// UDP needs no peer: nothing is ever sent to the bound port, so the
/// receiver parks in its 100 ms poll loop. Unlike `_close` (pinned by
/// `udp_close_cancels_first.rs`) `_cancel` does NOT free the handle, so the
/// reader's pointer stays valid throughout and the `_close` at the end is
/// the only teardown.
///
/// Bounded by a 10 s watchdog that FAILS. The rescue is a real datagram
/// burst (16 packets in one datagram — the `Receiver` TS syncer locks only
/// after four aligned packets) so the reader can always be joined; if even
/// that does not free it the test panics WITHOUT joining, because a failing
/// test must fail, never wedge.
#[test]
fn udp_receiver_cancel_wakes_parked_recv_with_closed() {
    let port = free_udp_port();
    let url = CString::new(format!("udp://127.0.0.1:{port}")).unwrap();
    let h = unsafe { tst_udp_recv_open(url.as_ptr()) };
    assert!(!h.is_null(), "tst_udp_recv_open failed: {}", unsafe {
        tst_get_last_error()
    });

    let (done_tx, done_rx) = mpsc::channel::<i32>();
    let reader_ptr = SendUdpRx(h);
    let reader = thread::spawn(move || {
        let p = reader_ptr; // whole-struct capture: SendUdpRx is Send, its field is not
        let mut buf = vec![0u8; 1316];
        let mut n = 0usize;
        let rc = unsafe { tst_udp_receiver_recv_ts(p.0, buf.as_mut_ptr(), buf.len(), &mut n) };
        let _ = done_tx.send(rc);
    });
    thread::sleep(Duration::from_millis(300)); // reader is parked on a poll tick

    let t0 = Instant::now();
    assert_eq!(unsafe { tst_udp_receiver_cancel(h) }, 0, "cancel rc");
    let rc = match done_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(rc) => rc,
        Err(_) => {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut pkt = [0xFFu8; 188];
            pkt[0] = 0x47;
            pkt[1] = 0x1F;
            pkt[2] = 0xFF;
            pkt[3] = 0x10;
            let mut burst = Vec::with_capacity(16 * 188);
            for _ in 0..16 {
                burst.extend_from_slice(&pkt);
            }
            let _ = s.send_to(&burst, ("127.0.0.1", port));
            if done_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                let _ = reader.join();
                unsafe { tst_udp_receiver_close(h) };
            }
            panic!("tst_udp_receiver_cancel did not wake the parked _recv_ts within 10 s");
        }
    };
    reader.join().expect("reader");
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "expected TST_E_CLOSED (-7) after cancel, got {rc} (woke after {:?})",
        t0.elapsed()
    );
    assert_eq!(
        unsafe { tst_udp_receiver_cancel(h) },
        0,
        "cancel is idempotent"
    );
    // `_cancel` never frees: the handle is still ours to close.
    unsafe { tst_udp_receiver_close(h) };
}

/// Every `tst_udp_*_cancel` returns 0 on a live handle and
/// `TST_E_INVALID_CONFIG` on NULL, and `_cancel` never consumes the handle
/// (the `_close` after it still frees).
#[test]
fn udp_cancel_entry_points_return_ok_and_null_is_invalid_config() {
    // Discard port (RFC 863): a UDP sender needs no peer to open.
    let url = CString::new("udp://127.0.0.1:9").unwrap();
    let s = unsafe { tst_udp_sender_open(url.as_ptr()) };
    assert!(!s.is_null(), "tst_udp_sender_open failed: {}", unsafe {
        tst_get_last_error()
    });
    assert_eq!(unsafe { tst_udp_sender_cancel(s) }, 0);
    assert_eq!(unsafe { tst_udp_sender_cancel(s) }, 0, "idempotent");
    unsafe { tst_udp_sender_close(s) };

    assert_eq!(
        unsafe { tst_udp_sender_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_udp_mux_sender_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_udp_receiver_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_udp_demux_receiver_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
}
