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
//! Cancel entry points (`tst_tcp_*_cancel`, ABI 0.22) are exercised below:
//! a cancel from another thread ends a parked recv/accept with
//! `TST_E_CLOSED`.
#![cfg(feature = "tcp")]

use std::ffi::CString;
use std::net::TcpListener as StdTcpListener;
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
use tstrans::tcp::{
    tst_tcp_demux_receiver_cancel, tst_tcp_demux_receiver_close,
    tst_tcp_demux_receiver_get_socket_stats, tst_tcp_demux_receiver_get_stats,
    tst_tcp_demux_receiver_get_stream_codec_stats, tst_tcp_demux_receiver_get_stream_stats,
    tst_tcp_demux_receiver_next_event, tst_tcp_demux_receiver_open,
    tst_tcp_demux_receiver_reset_stats, tst_tcp_listener_accept_receiver,
    tst_tcp_listener_accept_sender, tst_tcp_listener_bind, tst_tcp_listener_cancel,
    tst_tcp_listener_free, tst_tcp_mux_sender_cancel, tst_tcp_mux_sender_close,
    tst_tcp_mux_sender_finish, tst_tcp_mux_sender_get_mux_sender_stats,
    tst_tcp_mux_sender_get_socket_stats, tst_tcp_mux_sender_open, tst_tcp_mux_sender_push_video,
    tst_tcp_mux_sender_reset_stats, tst_tcp_receiver_cancel, tst_tcp_receiver_close,
    tst_tcp_receiver_get_socket_stats, tst_tcp_receiver_get_stats, tst_tcp_receiver_recv_ts,
    tst_tcp_receiver_reset_stats, tst_tcp_recv_open, tst_tcp_sender_cancel, tst_tcp_sender_close,
    tst_tcp_sender_get_socket_stats, tst_tcp_sender_get_stats, tst_tcp_sender_open,
    tst_tcp_sender_reset_stats, tst_tcp_sender_send_ts,
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
/// Nothing was ever pushed, so `pending_bytes` is empty and this takes the
/// EMPTY-DRAIN path: `finish` has nothing to send and closes. The rc is
/// asserted leniently only because the background peer has already dropped
/// its socket, so the transport's own `close()` may report an error the
/// shell does not surface; what must hold is that the call neither panics
/// nor mis-reports a null pointer, that a second call is 0, and that the
/// handle still frees with `_close`.
///
/// The drain-ERROR arm — `finish` reporting a failed drain — is pinned
/// separately by `tcp_mux_sender_finish_reports_the_drain_error`, which is
/// the only `_finish` test in this suite that reaches it.
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

// ---------------------------------------------------------------------------
// `_cancel` — Arc 2 R4, ABI 0.22
// ---------------------------------------------------------------------------

/// `_close` is documented callable from any thread; the raw pointer just
/// needs to cross the thread boundary to get there. `_cancel` is the same.
struct SendRx(*mut tstrans::tcp::TstTcpReceiver);
unsafe impl Send for SendRx {}

struct SendListener(*mut tstrans::tcp::TstTcpListener);
unsafe impl Send for SendListener {}

/// Ask the kernel for a free loopback TCP port by binding and releasing.
/// The C listener handle exposes no local-port getter, so a test that needs
/// to know the port must pick it first.
fn free_tcp_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .unwrap()
        .port()
}

/// Arc 2 R4: `tst_tcp_receiver_cancel` from another thread wakes a parked
/// `recv_ts` with `TST_E_CLOSED`.
///
/// The peer accepts and HOLDS the socket open without writing, so the
/// receiver genuinely parks in the read (a dropped peer would end it with
/// clean EOF instead, proving nothing). Bounded by a 10 s watchdog that
/// FAILS; the rescue drops the peer so the reader thread can always be
/// joined, and if even that does not free it the test panics WITHOUT
/// joining — a failing test must fail, never wedge.
#[test]
fn tcp_receiver_cancel_wakes_parked_recv_with_closed() {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().unwrap().port();
    let (hold_tx, hold_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let accepted = listener.accept();
        // Hold the accepted socket open until the test says it is done, so
        // the C receiver has no EOF to end on.
        let _ = hold_rx.recv_timeout(Duration::from_secs(15));
        drop(accepted);
    });

    let url = CString::new(format!("tcp://127.0.0.1:{port}")).unwrap();
    let h = unsafe { tst_tcp_recv_open(url.as_ptr()) };
    assert!(!h.is_null(), "tst_tcp_recv_open failed: {}", unsafe {
        tst_get_last_error()
    });

    let (done_tx, done_rx) = mpsc::channel::<i32>();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let reader_ptr = SendRx(h);
    let reader = thread::spawn(move || {
        let p = reader_ptr; // whole-struct capture: SendRx is Send, its field is not
        let mut buf = vec![0u8; 1316];
        let mut n = 0usize;
        // Signal BEFORE the call: the next statement enters the native recv
        // and does not return until it parks and is woken.
        let _ = entered_tx.send(());
        let rc = unsafe { tst_tcp_receiver_recv_ts(p.0, buf.as_mut_ptr(), buf.len(), &mut n) };
        let _ = done_tx.send(rc);
    });
    // Latch instead of a bare settle: wait for the reader to reach the call,
    // then give it one poll tick to be provably inside it.
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reader thread never reached _recv_ts");
    thread::sleep(Duration::from_millis(200));

    let t0 = Instant::now();
    assert_eq!(unsafe { tst_tcp_receiver_cancel(h) }, 0, "cancel rc");
    let rc = match done_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(rc) => rc,
        Err(_) => {
            // Rescue: drop the peer so the read ends on EOF and the reader
            // releases the slot; then fail loudly.
            let _ = hold_tx.send(());
            if done_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                let _ = reader.join();
                let _ = peer.join();
                unsafe { tst_tcp_receiver_close(h) };
            }
            panic!("tst_tcp_receiver_cancel did not wake the parked _recv_ts within 10 s");
        }
    };
    let _ = hold_tx.send(());
    reader.join().expect("reader");
    peer.join().expect("peer");
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "expected TST_E_CLOSED (-7) after cancel, got {rc} (woke after {:?})",
        t0.elapsed()
    );
    assert_eq!(
        unsafe { tst_tcp_receiver_cancel(h) },
        0,
        "cancel is idempotent"
    );
    unsafe { tst_tcp_receiver_close(h) };
}

/// Every `tst_tcp_*_cancel` returns 0 on a live handle and
/// `TST_E_INVALID_CONFIG` on NULL, and `_cancel` never consumes the handle
/// (the `_close` after it still frees).
#[test]
fn tcp_cancel_entry_points_return_ok_and_null_is_invalid_config() {
    let url_str = accept_one_background(|p| format!("tcp://127.0.0.1:{p}"));
    let url = CString::new(url_str).unwrap();
    let s = unsafe { tst_tcp_sender_open(url.as_ptr()) };
    assert!(!s.is_null(), "tst_tcp_sender_open failed");
    assert_eq!(unsafe { tst_tcp_sender_cancel(s) }, 0);
    assert_eq!(unsafe { tst_tcp_sender_cancel(s) }, 0, "idempotent");
    unsafe { tst_tcp_sender_close(s) };

    assert_eq!(
        unsafe { tst_tcp_sender_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_tcp_mux_sender_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_tcp_receiver_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_tcp_demux_receiver_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { tst_tcp_listener_cancel(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
}

/// `tst_tcp_listener_cancel` wakes a parked `accept_receiver` (Arc 1 WP-4b
/// gave `TcpListener` the handle; this is its C reach). The listener binds
/// on a port picked up front because the C ABI has no local-port getter —
/// the rescue path needs to be able to connect to it.
#[test]
fn tcp_listener_cancel_wakes_parked_accept() {
    let port = free_tcp_port();
    let bind = CString::new(format!("127.0.0.1:{port}")).unwrap();
    let l = unsafe { tst_tcp_listener_bind(bind.as_ptr()) };
    assert!(!l.is_null(), "tst_tcp_listener_bind failed: {}", unsafe {
        tst_get_last_error()
    });

    let (done_tx, done_rx) = mpsc::channel::<(bool, i32)>();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let lp = SendListener(l);
    let acceptor = thread::spawn(move || {
        let p = lp;
        // Signal BEFORE the call, as in the recv test above.
        let _ = entered_tx.send(());
        let r = unsafe { tst_tcp_listener_accept_receiver(p.0) };
        let code = unsafe { tst_get_last_error() };
        let _ = done_tx.send((r.is_null(), code));
        if !r.is_null() {
            unsafe { tst_tcp_receiver_close(r) };
        }
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("acceptor thread never reached _accept_receiver");
    thread::sleep(Duration::from_millis(200));

    let t0 = Instant::now();
    assert_eq!(unsafe { tst_tcp_listener_cancel(l) }, 0, "cancel rc");
    let (is_null, code) = match done_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(v) => v,
        Err(_) => {
            // Rescue: connect a peer so the accept returns and the thread
            // can be joined; then fail loudly.
            let _ = std::net::TcpStream::connect(("127.0.0.1", port));
            if done_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                let _ = acceptor.join();
                unsafe { tst_tcp_listener_free(l) };
            }
            panic!("tst_tcp_listener_cancel did not wake the parked accept within 10 s");
        }
    };
    acceptor.join().expect("acceptor");
    assert!(
        is_null,
        "accept must return NULL after cancel (woke after {:?})",
        t0.elapsed()
    );
    assert_eq!(
        code,
        TstError::Closed as i32,
        "expected TST_E_CLOSED (-7), got {code}"
    );
    assert_eq!(
        unsafe { tst_tcp_listener_cancel(l) },
        0,
        "cancel is idempotent"
    );
    unsafe { tst_tcp_listener_free(l) };
}

/// `_finish` REPORTS the drain error — the arm the other five `_finish`
/// tests cannot reach.
///
/// `MuxSender::finish` drains `pending_bytes`, and `pending_bytes` is
/// populated only by a send that FAILED (`Inner::drain_muxer` buffers the
/// chunk the transport rejected, plus whatever the muxer still holds). Every
/// other `_finish` test in this PR takes the empty-drain path and therefore
/// exercises only the `Ok(())` arm; this one pins the
/// `Err(e) => record_shell_error(&e)` projection end to end.
///
/// How the drain is made to fail deterministically: the peer accepts and
/// immediately drops its socket. The first write lands in the kernel send
/// buffer, the peer's stack answers RST, and a following write fails
/// (EPIPE / ECONNRESET). That failing push leaves the muxed bytes in
/// `pending_bytes`, so `_finish`'s drain re-sends them into the same dead
/// socket and surfaces the error.
///
/// Every wait is bounded: the push loop has both an iteration cap and a
/// wall-clock deadline and FAILS if no push ever errors, so the test can
/// never hang waiting for a failure that is not coming.
#[test]
fn tcp_mux_sender_finish_reports_the_drain_error() {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().unwrap().port();
    let (accepted_tx, accepted_rx) = mpsc::channel::<bool>();
    let peer = thread::spawn(move || {
        let ok = listener.accept().is_ok();
        // Drop the accepted socket immediately: the sender's next writes go
        // to a closed peer.
        let _ = accepted_tx.send(ok);
    });

    let url = CString::new(format!("tcp://127.0.0.1:{port}")).unwrap();
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };
    let h = unsafe { tst_tcp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!h.is_null(), "tst_tcp_mux_sender_open returned null");

    assert!(
        accepted_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("peer did not accept within 5 s"),
        "peer accept failed"
    );
    peer.join().expect("peer thread panicked");

    // Push until one push reports the dead peer. Bounded twice over.
    let nal = [0u8, 0, 0, 1, 0x65, 0xBB, 0x11, 0x22, 0x33, 0x44];
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut push_rc = 0;
    let mut pushes = 0u32;
    while pushes < 5_000 && Instant::now() < deadline {
        push_rc = unsafe {
            tst_tcp_mux_sender_push_video(
                h,
                nal.as_ptr(),
                nal.len(),
                i64::from(pushes) * 3_000,
                true,
            )
        };
        pushes += 1;
        if push_rc != 0 {
            break;
        }
    }
    assert_eq!(
        push_rc,
        TstError::Transport as i32,
        "the write that met the dead peer is a terminal transport error \
         (TST_E_TRANSPORT) after {pushes} pushes"
    );

    // The failed push retained its muxed bytes in `pending_bytes`, so
    // `_finish`'s drain re-sends into the same socket and must REPORT the
    // failure — not silently return 0.
    //
    // The code is TST_E_CLOSED, not the TST_E_TRANSPORT the push reported:
    // that terminal write LATCHED the transport dead
    // (`TcpTransport::dead_error`, `crates/tst-tcp/src/transport.rs`), so the
    // drain's send is refused at the dead-transport guard rather than
    // reaching the socket again. Both are the same `record_shell_error`
    // projection of a real `MuxSenderError` — this is the arm the five
    // empty-drain `_finish` tests never reach.
    let finish_rc = unsafe { tst_tcp_mux_sender_finish(h) };
    assert_eq!(
        finish_rc,
        TstError::Closed as i32,
        "_finish must report the drain failure as TST_E_CLOSED (the transport \
         latched dead on the failing push, which reported {push_rc})"
    );
    let msg = unsafe { std::ffi::CStr::from_ptr(tstrans::error::tst_get_last_error_str()) }
        .to_str()
        .unwrap_or("");
    assert!(
        !msg.is_empty(),
        "_finish must leave a last-error string describing the drain failure"
    );

    // `finish` closes the sender whatever the drain outcome, so the second
    // call takes the already-closed path and is quiet.
    assert_eq!(
        unsafe { tst_tcp_mux_sender_finish(h) },
        0,
        "second finish must be 0 even after a reported drain error"
    );

    unsafe { tst_tcp_mux_sender_close(h) };
}
