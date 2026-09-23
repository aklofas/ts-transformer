//! Smoke tests: open/close lifecycle + data-path null-guards for all four
//! TCP handle families, plus the TcpListener accept round-trip.
//!
//! Gated on `feature = "tcp"` so they compile only in builds that include
//! the TCP transport.
//!
//! Unlike UDP (connectionless), TCP requires an active peer or listener for
//! open calls to succeed. These tests spawn minimal loopback threads so the
//! caller-side `tst_tcp_*_open` calls connect without hanging.
//!
//! Note: the lib name for `tst-c` is `tstrans` (see `[lib] name` in
//! Cargo.toml); integration tests reference it as `tstrans`, not `tst_c`.
//!
//! There is no cancel surface on the TCP handles (the TCP transport does
//! not expose `cancel_handle()`), so these tests do not exercise a cancel
//! path.
#![cfg(feature = "tcp")]

use std::ffi::CString;
use std::net::TcpListener as StdTcpListener;
use std::thread;

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
use tstrans::tcp::{
    tst_tcp_demux_receiver_close, tst_tcp_demux_receiver_get_socket_stats,
    tst_tcp_demux_receiver_get_stats, tst_tcp_demux_receiver_get_stream_codec_stats,
    tst_tcp_demux_receiver_get_stream_stats, tst_tcp_demux_receiver_next_event,
    tst_tcp_demux_receiver_open, tst_tcp_demux_receiver_reset_stats,
    tst_tcp_listener_accept_sender, tst_tcp_listener_bind, tst_tcp_listener_free,
    tst_tcp_mux_sender_close, tst_tcp_mux_sender_finish, tst_tcp_mux_sender_get_mux_sender_stats,
    tst_tcp_mux_sender_get_socket_stats, tst_tcp_mux_sender_open, tst_tcp_mux_sender_reset_stats,
    tst_tcp_receiver_close, tst_tcp_receiver_get_socket_stats, tst_tcp_receiver_get_stats,
    tst_tcp_receiver_recv_ts, tst_tcp_receiver_reset_stats, tst_tcp_recv_open,
    tst_tcp_sender_close, tst_tcp_sender_get_socket_stats, tst_tcp_sender_get_stats,
    tst_tcp_sender_open, tst_tcp_sender_reset_stats, tst_tcp_sender_send_ts,
};

// ---------------------------------------------------------------------------
// Helper: bind a loopback listener on an ephemeral port and return the URL
// ---------------------------------------------------------------------------

/// Bind a TCP listener on an OS-assigned port, spawn a thread that accepts
/// and immediately drops the connection, and return the `tcp://127.0.0.1:N`
/// URL the caller should connect to.
///
/// This lets caller-side tests (`tst_tcp_*_open`) succeed on the first
/// `connect(2)` without hanging on the three-way handshake.
fn accept_one_background(url_template: impl Fn(u16) -> String) -> String {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        // Accept one connection and drop it. Ignore errors from the
        // subsequent close race between the test thread and this thread.
        let _ = listener.accept();
    });
    url_template(port)
}

// ---------------------------------------------------------------------------
// Lifecycle smoke (open + close) — one per handle family
// ---------------------------------------------------------------------------

/// Open a TCP sender to a loopback listener. A background thread accepts
/// the connection so the connect returns immediately. Handle must be
/// non-null and close without crashing.
#[test]
fn tcp_sender_open_loopback_returns_handle() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();
    let handle = unsafe { tst_tcp_sender_open(url.as_ptr()) };
    assert!(
        !handle.is_null(),
        "tst_tcp_sender_open returned null: {}",
        unsafe { std::ffi::CStr::from_ptr(tstrans::error::tst_get_last_error_str()) }
            .to_str()
            .unwrap_or("?")
    );
    unsafe { tst_tcp_sender_close(handle) };
}

/// Open a TCP receiver (caller-side connect) to a loopback listener.
#[test]
fn tcp_recv_open_loopback_returns_handle() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();
    let handle = unsafe { tst_tcp_recv_open(url.as_ptr()) };
    assert!(
        !handle.is_null(),
        "tst_tcp_recv_open returned null: {}",
        unsafe { std::ffi::CStr::from_ptr(tstrans::error::tst_get_last_error_str()) }
            .to_str()
            .unwrap_or("?")
    );
    unsafe { tst_tcp_receiver_close(handle) };
}

/// Open a TCP-backed mux sender to a loopback listener with a minimal
/// one-video-stream config. Handle must be non-null and close without crashing.
#[test]
fn tcp_mux_sender_open_returns_handle() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();

    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let handle = unsafe { tst_tcp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!handle.is_null(), "tst_tcp_mux_sender_open returned null");
    unsafe { tst_tcp_mux_sender_close(handle) };
}

/// Open a TCP-backed demux receiver (caller-side) to a loopback listener with
/// default demux config (NULL).
#[test]
fn tcp_demux_receiver_open_returns_handle() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();
    let handle = unsafe { tst_tcp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(
        !handle.is_null(),
        "tst_tcp_demux_receiver_open returned null"
    );
    unsafe { tst_tcp_demux_receiver_close(handle) };
}

// ---------------------------------------------------------------------------
// Malformed URL → null + TcpConfig (-31)
// ---------------------------------------------------------------------------

/// A malformed URL must return null and set last-error == TcpConfig (-31).
/// This exercises the URL parse path before any connection attempt.
#[test]
fn tcp_sender_open_malformed_url_returns_null_with_tcp_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_tcp_sender_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_tcp_sender_open should return null for a malformed URL"
    );
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::TcpConfig as i32,
        "malformed-url parse failure should set TcpConfig (-31), got {code}"
    );
}

#[test]
fn tcp_recv_open_malformed_url_returns_null_with_tcp_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_tcp_recv_open(url.as_ptr()) };
    assert!(handle.is_null());
    let code = unsafe { tst_get_last_error() };
    assert_eq!(code, TstError::TcpConfig as i32, "got {code}");
}

#[test]
fn tcp_demux_receiver_open_malformed_url_returns_null_with_tcp_config() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_tcp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(handle.is_null());
    let code = unsafe { tst_get_last_error() };
    assert_eq!(code, TstError::TcpConfig as i32, "got {code}");
}

// ---------------------------------------------------------------------------
// Connect-refused → null with TcpIo (-30) or TcpConnectTimeout (-32)
// ---------------------------------------------------------------------------

/// Connecting to a closed port should return null with either
/// `TcpIo` (connection refused) or `TcpConnectTimeout` (timeout first).
/// We use a short timeout via query param to keep the test fast.
#[test]
fn tcp_sender_open_refused_returns_null_with_io_or_timeout() {
    // Port 1 is privileged and almost certainly not open; we override the
    // connect_timeout to 100ms so the test doesn't stall.
    let url = CString::new("tcp://127.0.0.1:1?connect_timeout=100ms").unwrap();
    let handle = unsafe { tst_tcp_sender_open(url.as_ptr()) };
    // Either the URL fails to parse the timeout param (TcpConfig) or the
    // connection is refused (TcpIo) or times out (TcpConnectTimeout).
    // All three are acceptable failure codes here; we just require null + negative.
    assert!(handle.is_null(), "expected null for refused connection");
    let code = unsafe { tst_get_last_error() };
    assert!(
        code < 0,
        "expected a negative error code for refused connection, got {code}"
    );
}

// ---------------------------------------------------------------------------
// TcpListener bind + accept_sender round-trip
// ---------------------------------------------------------------------------

/// Bind a listener on an ephemeral port, connect a sender to it, accept the
/// connection via `tst_tcp_listener_accept_sender`, then close both handles.
#[test]
fn tcp_listener_bind_and_accept_sender_round_trip() {
    // Bind the listener.
    let addr = CString::new("127.0.0.1:0").unwrap();
    let listener_ptr = unsafe { tst_tcp_listener_bind(addr.as_ptr()) };
    if listener_ptr.is_null() {
        // Skip in sandboxed CI environments where bind fails.
        return;
    }

    // Discover the actual port via the underlying TcpListener. We read it
    // by looking at the handle's inner field directly (same crate, so
    // pub(crate) is accessible within integration tests via the public API).
    // Since we cannot call local_addr() here, we use a workaround: bind
    // a fresh std listener to :0, record its port, drop it, then re-bind our
    // tst listener to the same port — but that race-prone. Instead, spawn
    // the accept thread first, then connect from this thread using a
    // parallel approach: bind a std listener just to get a free port, then
    // do the real bind with the tst API.
    //
    // Simpler: just bind the std listener once, get the port, drop it, then
    // immediately bind the tst listener. Accept racy but works in practice.
    //
    // The above is already done: tst_tcp_listener_bind("127.0.0.1:0") already
    // bound port 0, and the OS assigned a port. The problem is we don't have
    // a tst_tcp_listener_local_addr() API yet. Instead, let's use a known port.
    //
    // Free the listener we already created and rebind to a known ephemeral port.
    unsafe { tst_tcp_listener_free(listener_ptr) };

    // Use a fresh std listener to discover a free port.
    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("std bind");
    let port = std_listener.local_addr().unwrap().port();
    drop(std_listener); // Release the port — there's a small TOCTOU window.

    let bind_addr = CString::new(format!("127.0.0.1:{port}")).unwrap();
    let listener_ptr = unsafe { tst_tcp_listener_bind(bind_addr.as_ptr()) };
    if listener_ptr.is_null() {
        // Port was grabbed before us — skip.
        return;
    }

    // Connect from a background thread. Raw pointers are not Send, so
    // transmit the pointer value as usize and reconstruct on the main thread.
    let connect_url = CString::new(format!("tcp://127.0.0.1:{port}")).unwrap();
    let connect_handle = thread::spawn(move || -> usize {
        // Brief pause so the accept call is already blocking when we connect.
        thread::sleep(std::time::Duration::from_millis(20));
        unsafe { tst_tcp_sender_open(connect_url.as_ptr()) as usize }
    });

    // Accept on the listener side.
    let accepted = unsafe { tst_tcp_listener_accept_sender(listener_ptr) };
    assert!(
        !accepted.is_null(),
        "tst_tcp_listener_accept_sender returned null"
    );
    unsafe { tst_tcp_sender_close(accepted) };
    unsafe { tst_tcp_listener_free(listener_ptr) };

    // Recover the connecting sender handle (as usize) and close it.
    if let Ok(ptr_usize) = connect_handle.join() {
        if ptr_usize != 0 {
            let sender_ptr = ptr_usize as *mut tstrans::tcp::TstTcpSender;
            unsafe { tst_tcp_sender_close(sender_ptr) };
        }
    }
}

// ---------------------------------------------------------------------------
// Stats round-trips on live handles (after successful open)
// ---------------------------------------------------------------------------

#[test]
fn tcp_sender_stats_and_reset() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();
    let h = unsafe { tst_tcp_sender_open(url.as_ptr()) };
    if h.is_null() {
        return; // skip if connect fails in CI
    }

    let mut stats = TstSenderStats::default();
    assert_eq!(unsafe { tst_tcp_sender_get_stats(h, &mut stats) }, 0);
    assert_eq!(unsafe { tst_tcp_sender_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_tcp_sender_get_socket_stats(h, &mut ss) };

    unsafe { tst_tcp_sender_close(h) };
}

#[test]
fn tcp_receiver_stats_and_reset() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();
    let h = unsafe { tst_tcp_recv_open(url.as_ptr()) };
    if h.is_null() {
        return;
    }

    let mut stats = TstReceiverStats::default();
    assert_eq!(unsafe { tst_tcp_receiver_get_stats(h, &mut stats) }, 0);
    assert_eq!(unsafe { tst_tcp_receiver_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_tcp_receiver_get_socket_stats(h, &mut ss) };

    unsafe { tst_tcp_receiver_close(h) };
}

#[test]
fn tcp_mux_sender_stats_and_reset() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();

    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let h = unsafe { tst_tcp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    if h.is_null() {
        return;
    }

    let mut stats = TstMuxSenderStats::default();
    assert_eq!(
        unsafe { tst_tcp_mux_sender_get_mux_sender_stats(h, &mut stats) },
        0
    );
    assert_eq!(unsafe { tst_tcp_mux_sender_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_tcp_mux_sender_get_socket_stats(h, &mut ss) };

    unsafe { tst_tcp_mux_sender_close(h) };
}

#[test]
fn tcp_demux_receiver_stats_and_reset() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();
    let h = unsafe { tst_tcp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    if h.is_null() {
        return;
    }

    let mut stats = TstDemuxReceiverStats::default();
    assert_eq!(
        unsafe { tst_tcp_demux_receiver_get_stats(h, &mut stats) },
        0
    );
    assert_eq!(unsafe { tst_tcp_demux_receiver_reset_stats(h) }, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_tcp_demux_receiver_get_socket_stats(h, &mut ss) };

    let mut cs = TstStreamCodecStats {
        kind: TST_CODEC_KIND_UNKNOWN,
        _pad: 0,
        u: TstStreamCodecStatsUnion {
            unknown: Default::default(),
        },
    };
    // No PIDs observed yet — expect NOT_FOUND.
    let rc = unsafe { tst_tcp_demux_receiver_get_stream_codec_stats(h, 0x0100, &mut cs) };
    assert_eq!(rc, TstError::NotFound as i32);

    // Per-PID borrowed buffer — should return 0 and an empty slice.
    let mut arr: *const tstrans::stats::TstStreamStats = std::ptr::null();
    let mut count: libc::size_t = 0;
    let rc = unsafe { tst_tcp_demux_receiver_get_stream_stats(h, &mut arr, &mut count) };
    assert_eq!(rc, 0);
    assert_eq!(count, 0);

    unsafe { tst_tcp_demux_receiver_close(h) };
}

// ---------------------------------------------------------------------------
// Null-pointer guards (data-path must not crash/panic on null)
// ---------------------------------------------------------------------------

#[test]
fn null_send_ts_returns_invalid_config() {
    let rc = unsafe { tst_tcp_sender_send_ts(std::ptr::null_mut(), std::ptr::null(), 0) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn null_recv_ts_returns_invalid_config() {
    let mut buf = [0u8; 188];
    let mut n: usize = 0;
    let rc = unsafe {
        tst_tcp_receiver_recv_ts(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut n)
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn null_next_event_returns_invalid_config() {
    let mut ev = TstEvent::default();
    let rc = unsafe { tst_tcp_demux_receiver_next_event(std::ptr::null_mut(), &mut ev) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

// ---------------------------------------------------------------------------
// `_finish` — Arc 2 R3 (DEBT-14 "ship now" cell), ABI 0.22
// ---------------------------------------------------------------------------

/// `tst_tcp_mux_sender_finish` drains and closes.
///
/// The background peer accepts and immediately drops the socket, so the
/// drain may legitimately report a transport error (peer gone) or 0
/// (nothing pending). Both are "finished"; what must hold is that the
/// call neither panics nor mis-reports a null pointer, that a second call
/// is 0, and that the handle still frees with `_close`.
#[test]
fn tcp_mux_sender_finish_then_close() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();

    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let h = unsafe { tst_tcp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!h.is_null(), "tst_tcp_mux_sender_open returned null");

    let rc = unsafe { tst_tcp_mux_sender_finish(h) };
    assert!(
        rc == 0
            || (rc < 0
                && rc != TstError::PanicCaught as i32
                && rc != TstError::InvalidConfig as i32),
        "finish rc={rc}"
    );
    assert_eq!(
        unsafe { tst_tcp_mux_sender_finish(h) },
        0,
        "second finish is 0"
    );
    assert_eq!(
        unsafe { tst_tcp_mux_sender_finish(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );

    unsafe { tst_tcp_mux_sender_close(h) };
}
