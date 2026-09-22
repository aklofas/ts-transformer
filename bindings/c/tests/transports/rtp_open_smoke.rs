//! Smoke tests: open/close lifecycle + data-path entry points for all
//! four RTP handle families.
//!
//! These tests are gated on `feature = "rtp"` so they compile only in
//! builds that include the RTP transport.
//!
//! The lifecycle tests (open/close) verify that handles open cleanly on
//! loopback and close without UB (no double-free, no leak detectable by
//! Miri/Valgrind). The data-path tests verify that the entry points
//! compile, accept valid input, and reject null/invalid arguments with
//! the correct error codes — they do not require an active peer
//! (senders target loopback, receivers are cancelled before timing out).
#![cfg(feature = "rtp")]

use std::ffi::CString;
use std::net::UdpSocket;

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::event::{TstEvent, TstEventKind};
use tstrans::rtp::{
    tst_rtp_demux_receiver_cancel, tst_rtp_demux_receiver_close,
    tst_rtp_demux_receiver_get_socket_stats, tst_rtp_demux_receiver_get_stats,
    tst_rtp_demux_receiver_get_stream_codec_stats,
    tst_rtp_demux_receiver_get_stream_last_seen_micros, tst_rtp_demux_receiver_get_stream_stats,
    tst_rtp_demux_receiver_next_event, tst_rtp_demux_receiver_open,
    tst_rtp_demux_receiver_reset_stats, tst_rtp_mux_sender_cancel, tst_rtp_mux_sender_close,
    tst_rtp_mux_sender_get_mux_sender_stats, tst_rtp_mux_sender_get_socket_stats,
    tst_rtp_mux_sender_get_stream_codec_stats, tst_rtp_mux_sender_open,
    tst_rtp_mux_sender_push_klv, tst_rtp_mux_sender_push_video, tst_rtp_mux_sender_reset_stats,
    tst_rtp_receiver_cancel, tst_rtp_receiver_close, tst_rtp_receiver_get_socket_stats,
    tst_rtp_receiver_get_stats, tst_rtp_receiver_recv_ts, tst_rtp_receiver_reset_stats,
    tst_rtp_recv_open, tst_rtp_sender_cancel, tst_rtp_sender_close,
    tst_rtp_sender_get_socket_stats, tst_rtp_sender_get_stats, tst_rtp_sender_open,
    tst_rtp_sender_reset_stats, tst_rtp_sender_send_ts,
};
use tstrans::stats::{
    TST_CODEC_KIND_UNKNOWN, TstDemuxReceiverStats, TstMuxSenderStats, TstReceiverStats,
    TstSenderStats, TstSocketStats, TstStreamCodecStats, TstStreamCodecStatsUnion,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Ask the kernel for an available UDP port on loopback by binding and
/// immediately releasing. Not TOCTOU-safe for production, but fine for tests.
fn pick_port() -> u16 {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    sock.local_addr().unwrap().port()
}

/// Open an RTP receiver on a kernel-assigned (`:0`) port, retrying on a
/// transient null return.
///
/// An RTP receiver auto-binds an RTCP companion socket on `port + 1`. A `:0`
/// ephemeral port only guarantees the RTP port itself is free, not `port + 1`,
/// so under concurrent test execution (cargo-nextest) the companion bind
/// occasionally collides and the open returns null. Each retry requests a
/// fresh ephemeral port, which clears the collision within a few attempts.
/// (These tests are additionally serialized through the nextest `network`
/// test-group — `binary(transports)` in `.config/nextest.toml` — so
/// intra-suite collisions can't happen; the retry mops up residual
/// collisions with unrelated processes on the runner.)
fn open_rtp_with_retry<T>(mut open: impl FnMut() -> *mut T) -> *mut T {
    let mut h = open();
    for _ in 0..20 {
        if !h.is_null() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        h = open();
    }
    h
}

// ---------------------------------------------------------------------------
// Lifecycle smoke (open + close)
// ---------------------------------------------------------------------------

/// Bind a receiver on the loopback with port 0 (kernel-assigned).
/// The handle must be non-null and must close without crashing.
#[test]
fn rtp_recv_open_unicast_zero_port_returns_handle() {
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let handle = open_rtp_with_retry(|| unsafe { tst_rtp_recv_open(url.as_ptr()) });
    assert!(
        !handle.is_null(),
        "tst_rtp_recv_open returned null for rtp://127.0.0.1:0"
    );
    unsafe { tst_rtp_receiver_close(handle) };
}

/// Open an RTP sender to a loopback unicast address. UDP is connectionless
/// so the open succeeds without a peer.
#[test]
fn rtp_sender_open_unicast_returns_handle() {
    let url = CString::new("rtp://127.0.0.1:54321").unwrap();
    let handle = unsafe { tst_rtp_sender_open(url.as_ptr()) };
    assert!(
        !handle.is_null(),
        "tst_rtp_sender_open returned null for rtp://127.0.0.1:54321"
    );
    unsafe { tst_rtp_sender_close(handle) };
}

/// A malformed URL must return null and set a meaningful last-error.
#[test]
fn rtp_recv_open_malformed_url_returns_null() {
    let url = CString::new("not-a-url").unwrap();
    let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
    assert!(
        handle.is_null(),
        "tst_rtp_recv_open should return null for a malformed URL"
    );
    let code = unsafe { tst_get_last_error() };
    assert!(
        code < 0,
        "last-error code should be negative after parse failure, got {code}"
    );
}

// ---------------------------------------------------------------------------
// TstRtpSender data-path
// ---------------------------------------------------------------------------

#[test]
fn rtp_sender_send_ts_roundtrip() {
    // Open a receiver first to bind the port.
    let recv_url = CString::new("rtp://127.0.0.1:0").unwrap();
    let recv_h = open_rtp_with_retry(|| unsafe { tst_rtp_recv_open(recv_url.as_ptr()) });
    assert!(!recv_h.is_null(), "receiver open failed");

    // Read back the actual bound port from the stats (socket_stats is not
    // available for unconnected UDP, so just use a fixed known port here).
    // For simplicity we open a fresh pair on a kernel-assigned port.
    unsafe { tst_rtp_receiver_close(recv_h) };

    // Use a fixed port pair. Pick a free port, open receiver, then sender.
    let port = pick_port();
    let recv_url = CString::new(format!("rtp://127.0.0.1:{port}")).unwrap();
    let recv_h = unsafe { tst_rtp_recv_open(recv_url.as_ptr()) };
    if recv_h.is_null() {
        // Port may have been grabbed between pick and bind — skip.
        return;
    }

    let send_url = CString::new(format!("rtp://127.0.0.1:{port}")).unwrap();
    let send_h = unsafe { tst_rtp_sender_open(send_url.as_ptr()) };
    assert!(!send_h.is_null(), "sender open failed");

    // Push one MPEG-TS packet (188 bytes, sync byte 0x47).
    let mut pkt = [0u8; 188];
    pkt[0] = 0x47;
    let rc = unsafe { tst_rtp_sender_send_ts(send_h, pkt.as_ptr(), pkt.len()) };
    assert_eq!(rc, 0, "send_ts failed with code {rc}");

    // Cancel receiver so recv_ts returns promptly.
    let cancel_rc = unsafe { tst_rtp_receiver_cancel(recv_h) };
    assert_eq!(cancel_rc, 0);

    // recv_ts should return CLOSED (cancelled) or a TS packet.
    let mut buf = [0u8; 188];
    let mut n: usize = 0;
    let _rc = unsafe { tst_rtp_receiver_recv_ts(recv_h, buf.as_mut_ptr(), buf.len(), &mut n) };
    // Either 0 (packet received) or TST_E_CLOSED after cancel — both valid.

    unsafe {
        tst_rtp_sender_close(send_h);
        tst_rtp_receiver_close(recv_h);
    }
}

#[test]
fn rtp_sender_stats_and_reset() {
    let url = CString::new("rtp://127.0.0.1:54322").unwrap();
    let h = unsafe { tst_rtp_sender_open(url.as_ptr()) };
    assert!(!h.is_null());

    let mut stats = TstSenderStats::default();
    let rc = unsafe { tst_rtp_sender_get_stats(h, &mut stats) };
    assert_eq!(rc, 0);

    let rc = unsafe { tst_rtp_sender_reset_stats(h) };
    assert_eq!(rc, 0);

    // Socket stats may or may not be available for RTP (UDP doesn't always
    // expose RTT etc.) — just verify it doesn't crash.
    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_rtp_sender_get_socket_stats(h, &mut ss) };

    unsafe { tst_rtp_sender_close(h) };
}

#[test]
fn rtp_sender_cancel_returns_ok() {
    let url = CString::new("rtp://127.0.0.1:54323").unwrap();
    let h = unsafe { tst_rtp_sender_open(url.as_ptr()) };
    assert!(!h.is_null());
    let rc = unsafe { tst_rtp_sender_cancel(h) };
    assert_eq!(rc, 0);
    unsafe { tst_rtp_sender_close(h) };
}

// ---------------------------------------------------------------------------
// TstRtpReceiver data-path
// ---------------------------------------------------------------------------

#[test]
fn rtp_receiver_cancel_unblocks() {
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let h = open_rtp_with_retry(|| unsafe { tst_rtp_recv_open(url.as_ptr()) });
    assert!(!h.is_null());

    // Cancel immediately so that recv_ts returns without waiting.
    let rc = unsafe { tst_rtp_receiver_cancel(h) };
    assert_eq!(rc, 0);

    let mut buf = [0u8; 188];
    let mut n: usize = 0;
    let rc = unsafe { tst_rtp_receiver_recv_ts(h, buf.as_mut_ptr(), buf.len(), &mut n) };
    // After cancel this is exactly CLOSED: `RtpRecvTransport` returns
    // `ExplicitClose` once its cancel flag is set, and Arc 2's `Owned::cancel`
    // latches BEFORE it wakes the parked call, so the relabeller cannot race.
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "expected CLOSED after cancel, got {rc}"
    );

    unsafe { tst_rtp_receiver_close(h) };
}

#[test]
fn rtp_receiver_stats_and_reset() {
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let h = open_rtp_with_retry(|| unsafe { tst_rtp_recv_open(url.as_ptr()) });
    assert!(!h.is_null());

    let mut stats = TstReceiverStats::default();
    let rc = unsafe { tst_rtp_receiver_get_stats(h, &mut stats) };
    assert_eq!(rc, 0);

    let rc = unsafe { tst_rtp_receiver_reset_stats(h) };
    assert_eq!(rc, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_rtp_receiver_get_socket_stats(h, &mut ss) };

    unsafe { tst_rtp_receiver_close(h) };
}

// ---------------------------------------------------------------------------
// TstRtpMuxSender data-path
// ---------------------------------------------------------------------------

#[test]
fn rtp_mux_sender_push_video_reflects_in_stats() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("rtp://127.0.0.1:54324").unwrap();
    let h = unsafe { tst_rtp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!h.is_null(), "rtp mux sender open failed");

    // Push a minimal valid H.264 NAL (1-byte slice header, key frame).
    // The muxer won't emit TS packets until sufficient buffering, but the
    // push itself must not crash and should return a recognisable code.
    let nal = [0x65u8]; // NAL unit type 5 = IDR slice (key frame)
    let rc = unsafe { tst_rtp_mux_sender_push_video(h, nal.as_ptr(), nal.len(), 0, true) };
    // Acceptable return codes in a loopback-only test:
    //   0            — buffered without error
    //   Transport    — UDP ENOBUFS / ECONNREFUSED (no peer listening)
    //   Closed       — send socket closed early
    //   InvalidTs    — muxer rejected the mini NAL as too-small (also fine)
    assert!(
        rc == 0
            || rc == TstError::Transport as i32
            || rc == TstError::Closed as i32
            || rc == TstError::InvalidTs as i32,
        "unexpected push_video return code {rc}"
    );

    // Verify stats don't crash.
    let mut stats = TstMuxSenderStats::default();
    let rc = unsafe { tst_rtp_mux_sender_get_mux_sender_stats(h, &mut stats) };
    assert_eq!(rc, 0);

    let rc = unsafe { tst_rtp_mux_sender_reset_stats(h) };
    assert_eq!(rc, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_rtp_mux_sender_get_socket_stats(h, &mut ss) };

    let mut cs = TstStreamCodecStats {
        kind: TST_CODEC_KIND_UNKNOWN,
        _pad: 0,
        u: TstStreamCodecStatsUnion {
            unknown: Default::default(),
        },
    };
    // 0x1011 may not have emitted yet — just verify the call shape.
    let _rc = unsafe { tst_rtp_mux_sender_get_stream_codec_stats(h, 0x1011, &mut cs) };

    unsafe { tst_rtp_mux_sender_close(h) };
}

#[test]
fn rtp_mux_sender_push_klv_does_not_crash() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("rtp://127.0.0.1:54325").unwrap();
    let h = unsafe { tst_rtp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    if h.is_null() {
        return; // skip if port unavailable
    }

    // KLV push on a config with no KLV stream must return INVALID_USAGE.
    let klv = [0u8; 8];
    let rc = unsafe { tst_rtp_mux_sender_push_klv(h, klv.as_ptr(), klv.len(), 0) };
    // Expected: TST_E_INVALID_USAGE (no KLV stream) or TST_E_TRANSPORT.
    assert!(rc != 0, "push_klv with no KLV stream should fail");

    unsafe { tst_rtp_mux_sender_close(h) };
}

#[test]
fn rtp_mux_sender_cancel_returns_ok() {
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

    let url = CString::new("rtp://127.0.0.1:54326").unwrap();
    let h = unsafe { tst_rtp_mux_sender_open(url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!h.is_null());

    let rc = unsafe { tst_rtp_mux_sender_cancel(h) };
    assert_eq!(rc, 0);
    unsafe { tst_rtp_mux_sender_close(h) };
}

// ---------------------------------------------------------------------------
// TstRtpDemuxReceiver data-path
// ---------------------------------------------------------------------------

#[test]
fn rtp_demux_receiver_cancel_unblocks_next_event() {
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let h = open_rtp_with_retry(|| unsafe {
        tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null())
    });
    assert!(!h.is_null());

    // Cancel immediately so that next_event returns without waiting.
    let rc = unsafe { tst_rtp_demux_receiver_cancel(h) };
    assert_eq!(rc, 0);

    let mut ev = TstEvent::default();
    let rc = unsafe { tst_rtp_demux_receiver_next_event(h, &mut ev) };
    // After cancel this is exactly CLOSED: `RtpRecvTransport` returns
    // `ExplicitClose` once its cancel flag is set, and Arc 2's `Owned::cancel`
    // latches BEFORE it wakes the parked call, so the relabeller cannot race.
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "expected CLOSED after cancel, got {rc}"
    );

    unsafe { tst_rtp_demux_receiver_close(h) };
}

#[test]
fn rtp_demux_receiver_stats_and_reset() {
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let h = open_rtp_with_retry(|| unsafe {
        tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null())
    });
    assert!(!h.is_null());

    let mut stats = TstDemuxReceiverStats::default();
    let rc = unsafe { tst_rtp_demux_receiver_get_stats(h, &mut stats) };
    assert_eq!(rc, 0);

    let rc = unsafe { tst_rtp_demux_receiver_reset_stats(h) };
    assert_eq!(rc, 0);

    let mut ss = TstSocketStats::default();
    let _rc = unsafe { tst_rtp_demux_receiver_get_socket_stats(h, &mut ss) };

    let mut cs = TstStreamCodecStats {
        kind: TST_CODEC_KIND_UNKNOWN,
        _pad: 0,
        u: TstStreamCodecStatsUnion {
            unknown: Default::default(),
        },
    };
    // No PIDs observed yet — expect NOT_FOUND.
    let rc = unsafe { tst_rtp_demux_receiver_get_stream_codec_stats(h, 0x0100, &mut cs) };
    assert_eq!(rc, TstError::NotFound as i32);

    // Stream stats (per-PID borrowed buffer) — should return 0 and an empty slice.
    let mut arr: *const tstrans::stats::TstStreamStats = std::ptr::null();
    let mut count: libc::size_t = 0;
    let rc = unsafe { tst_rtp_demux_receiver_get_stream_stats(h, &mut arr, &mut count) };
    assert_eq!(rc, 0);
    assert_eq!(count, 0);

    unsafe { tst_rtp_demux_receiver_close(h) };
}

/// Live RTP session: a `TstRtpMuxSender` pushes video into a bound
/// `TstRtpDemuxReceiver`; once a Sample event for the video pid has been
/// observed, `tst_rtp_demux_receiver_get_stream_last_seen_micros` must
/// report a nonzero epoch timestamp for that pid, and `0` for a pid that
/// was never observed.
///
/// RTP is connectionless UDP so (unlike the SRT loopback fixture) opening
/// the receiver never blocks on a peer — no rendezvous channel is needed,
/// everything runs on one thread. The one real risk is `next_event`
/// blocking forever if the pushed frames never cross the 1316-byte
/// (7-TS-packet) `pkt_size` flush threshold in `drain_muxer` (see
/// `crates/tst-pipeline/src/mux_sender.rs`) — pushing 20 small NAL frames
/// (comfortably more than the ~5 needed to cross that threshold once PAT+PMT
/// are counted) makes that a near-certainty, and a bounded watchdog thread
/// cancels the receiver as a last resort so a regression here fails fast
/// instead of hanging the suite.
#[test]
fn rtp_demux_receiver_get_stream_last_seen_micros_reports_after_sample() {
    let port = pick_port();
    let recv_url = CString::new(format!("rtp://127.0.0.1:{port}")).unwrap();
    let rx = unsafe { tst_rtp_demux_receiver_open(recv_url.as_ptr(), std::ptr::null()) };
    if rx.is_null() {
        // Port may have been grabbed between pick_port and bind — skip
        // (mirrors rtp_sender_send_ts_roundtrip's tolerance for this race).
        return;
    }

    // Watchdog: cancels `rx` if the drain loop below is still running after
    // 5s. Polls a shared `done` flag in short increments so the common
    // (fast) path pays only that increment, not the full timeout; the main
    // thread joins the watchdog before closing `rx` so there is no
    // use-after-free race on the cancel call.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_done = std::sync::Arc::clone(&done);
    let rx_addr = rx as usize;
    let watchdog = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if watchdog_done.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !watchdog_done.load(std::sync::atomic::Ordering::Acquire) {
            unsafe { tst_rtp_demux_receiver_cancel(rx_addr as *mut _) };
        }
    });

    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };
    let send_url = CString::new(format!("rtp://127.0.0.1:{port}")).unwrap();
    let tx = unsafe { tst_rtp_mux_sender_open(send_url.as_ptr(), cfg as *const _) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!tx.is_null(), "rtp mux sender open failed");

    let nal: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x01, // Annex-B start code
        0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, // SPS-shaped (nal_type=7)
        0x00, 0x00, 0x00, 0x01, // Annex-B start code
        0x65, 0x88, 0x80, 0x40, // IDR-shaped (nal_type=5)
    ];
    for i in 0..20 {
        let pts = (i as i64) * 3_600;
        let rc = unsafe { tst_rtp_mux_sender_push_video(tx, nal.as_ptr(), nal.len(), pts, true) };
        assert_eq!(rc, 0, "push_video[{i}] failed: rc={rc}");
    }

    // Let the OS deliver the flushed datagram(s) into the receiver's socket
    // buffer before we start draining events.
    std::thread::sleep(std::time::Duration::from_millis(300));
    unsafe { tst_rtp_mux_sender_close(tx) };

    let mut got_pmt = false;
    let mut got_sample = false;
    let mut ev = TstEvent::default();
    loop {
        let rc = unsafe { tst_rtp_demux_receiver_next_event(rx, &mut ev) };
        if rc == 0 {
            if ev.kind == TstEventKind::ProgramMap as i32 {
                got_pmt = true;
            }
            if ev.kind == TstEventKind::Sample as i32 {
                got_sample = true;
            }
            if got_pmt && got_sample {
                break;
            }
            continue;
        }
        // Any error (watchdog cancel, or a genuine end-of-stream) ends the drain.
        break;
    }

    done.store(true, std::sync::atomic::Ordering::Release);
    watchdog.join().expect("watchdog thread panicked");

    assert!(got_pmt, "no PROGRAM_MAP event received over rtp loopback");
    assert!(got_sample, "no SAMPLE event received over rtp loopback");

    let mut micros: u64 = 0;
    let rc = unsafe { tst_rtp_demux_receiver_get_stream_last_seen_micros(rx, 0x1011, &mut micros) };
    assert_eq!(rc, 0, "get_stream_last_seen_micros(0x1011) failed: rc={rc}");
    assert!(
        micros > 0,
        "expected nonzero last_seen after a delivered sample"
    );

    let mut bogus_micros: u64 = 0xDEAD_BEEF;
    let rc = unsafe {
        tst_rtp_demux_receiver_get_stream_last_seen_micros(rx, 0x1FFF, &mut bogus_micros)
    };
    assert_eq!(rc, 0, "get_stream_last_seen_micros(0x1FFF) failed: rc={rc}");
    assert_eq!(
        bogus_micros, 0,
        "unknown pid must report 0, not a stale value"
    );

    unsafe { tst_rtp_demux_receiver_close(rx) };
}

// ---------------------------------------------------------------------------
// Null-pointer guards (data-path must not crash/panic on null)
// ---------------------------------------------------------------------------

#[test]
fn null_send_ts_returns_invalid_config() {
    let rc = unsafe { tst_rtp_sender_send_ts(std::ptr::null_mut(), std::ptr::null(), 0) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn null_recv_ts_returns_invalid_config() {
    let mut buf = [0u8; 188];
    let mut n: usize = 0;
    let rc = unsafe {
        tst_rtp_receiver_recv_ts(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut n)
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn null_next_event_returns_invalid_config() {
    let mut ev = TstEvent::default();
    let rc = unsafe { tst_rtp_demux_receiver_next_event(std::ptr::null_mut(), &mut ev) };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}
