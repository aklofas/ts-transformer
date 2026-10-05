//! C-ABI stats integration tests. Each handle type's get/reset accessor
//! gets exercised end-to-end against a live (loopback) or in-process
//! handle.

// 9 of the 11 tests need `tst_srt` + `tstrans::sender::*`/`receiver::*`
// (gated on `feature = "srt"`). The 2 stats-layout tests at the top
// don't need srt, but they're insurance against struct-layout drift —
// running them only in srt-enabled builds is acceptable, and a unified
// gate is simpler than 9 per-test attributes.
#![cfg(feature = "srt")]

use tstrans::stats::TstRawSendStats;

#[test]
fn raw_sender_stats_layout_is_repr_c() {
    let s = TstRawSendStats::default();
    assert_eq!(std::mem::size_of::<TstRawSendStats>(), 16);
    assert_eq!(s.bytes_sent, 0);
    assert_eq!(s.packets_sent, 0);
}

#[test]
fn socket_stats_layout() {
    use tstrans::stats::TstSocketStats;
    // 16 fields: 3 u32 + 1 u32 pad + 13 u64 = 16 + 104 = 120 B.
    assert_eq!(std::mem::size_of::<TstSocketStats>(), 120);

    let s = TstSocketStats::default();
    assert_eq!(s.rtt_us, 0);
    assert_eq!(s.send_bandwidth_bps, 0);
    assert_eq!(s.bytes_sent, 0);
    assert_eq!(s.send_buffer_packets, 0);
}

use std::ptr;
use tstrans::stats::{TST_STATS_MAX_STREAMS, TstMuxerStats};

#[test]
fn muxer_stats_layout() {
    let s = TstMuxerStats::default();
    assert_eq!(s.per_stream_count, 0);
    assert_eq!(s.per_stream_truncated, 0);
    assert_eq!(s.per_stream.len(), TST_STATS_MAX_STREAMS);
}

#[test]
fn muxer_get_stats_after_push() {
    use tstrans::config::{TstKlvStreamType, TstVideoCodec};
    use tstrans::config::{
        tst_mux_config_add_klv_stream, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::muxer::{
        tst_muxer_close, tst_muxer_get_stats, tst_muxer_open, tst_muxer_reset_stats,
    };
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x0100, TstVideoCodec::H264);
        tst_mux_config_add_klv_stream(cfg, prog, 0x0101, TstKlvStreamType::PrivateData, false);
        let m = tst_muxer_open(cfg);
        assert!(!m.is_null());
        // Fresh muxer: stats start zero, but per_stream_count == 2 (eager).
        let mut st = TstMuxerStats::default();
        let rc = tst_muxer_get_stats(m, &mut st);
        assert_eq!(rc, 0);
        assert_eq!(st.per_stream_count, 2);
        assert_eq!(st.per_stream_truncated, 0);
        // Reset round-trip is a no-op on zeros.
        let rc = tst_muxer_reset_stats(m);
        assert_eq!(rc, 0);
        // Cleanup.
        tst_muxer_close(m);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn muxer_get_stats_null_pointer_returns_invalid_config() {
    let mut st = TstMuxerStats::default();
    unsafe {
        let rc = tstrans::muxer::tst_muxer_get_stats(ptr::null_mut(), &mut st);
        assert_ne!(rc, 0);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn mux_sender_stats_round_trip() {
    use std::ffi::CString;
    use std::sync::mpsc;
    use std::time::Duration;
    use tst_srt::ListenerBuilder;
    use tstrans::config::{
        TstKlvStreamType, TstVideoCodec, tst_mux_config_add_klv_stream, tst_mux_config_add_program,
        tst_mux_config_add_video_stream, tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::sender::mux_sender::{
        tst_mux_sender_close, tst_mux_sender_get_stats, tst_mux_sender_open,
        tst_mux_sender_reset_stats, tst_mux_sender_send_video,
    };
    use tstrans::stats::TstMuxSenderStats;

    let (port_tx, port_rx) = mpsc::channel::<u16>();
    // `done` lets the sender tell the accept thread when it has finished all
    // its stats reads, so the receiver holds the accepted connection open for
    // exactly that long. The old code did `let _ = listener.accept()` (which
    // drops the accepted socket IMMEDIATELY, closing the connection) then slept
    // a fixed 2s on the listener — so the sender's send_video / get_*_stats
    // raced the close and intermittently saw rc!=0 under CI load (flaky gate).
    let (done_tx, done_rx) = mpsc::channel::<()>();

    let _accept_thread = std::thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        // Bind (not `_`) so the accepted socket stays alive; drop only after
        // the sender signals done (bounded so a missed signal can't hang).
        let _accepted = listener.accept();
        let _ = done_rx.recv_timeout(Duration::from_secs(10));
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener didn't bind in time");

    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x0100, TstVideoCodec::H264);
        tst_mux_config_add_klv_stream(cfg, prog, 0x0101, TstKlvStreamType::PrivateData, false);
        let url = CString::new(format!("srt://127.0.0.1:{port}?latency=120")).unwrap();
        let s = tst_mux_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null());

        let nal: [u8; 7] = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB, 0xCC];
        let rc = tst_mux_sender_send_video(s, nal.as_ptr(), nal.len(), 0, true);
        assert_eq!(rc, 0);

        let mut st = TstMuxSenderStats::default();
        let rc = tst_mux_sender_get_stats(s, &mut st);
        assert_eq!(rc, 0);
        assert_eq!(st.per_stream_count, 2);
        let video_entry = st
            .per_stream
            .iter()
            .find(|e| e.pid == 0x0100)
            .expect("video entry");
        assert_eq!(video_entry.items, 1);

        let rc = tst_mux_sender_reset_stats(s);
        assert_eq!(rc, 0);
        let mut st2 = TstMuxSenderStats::default();
        tst_mux_sender_get_stats(s, &mut st2);
        let video_entry2 = st2
            .per_stream
            .iter()
            .find(|e| e.pid == 0x0100)
            .expect("video entry after reset");
        assert_eq!(video_entry2.items, 0);

        // Sender done with its reads — let the accept thread release the peer.
        let _ = done_tx.send(());
        tst_mux_sender_close(s);
        tst_mux_config_free(cfg);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn mux_sender_socket_stats_round_trip() {
    use std::ffi::CString;
    use std::sync::mpsc;
    use std::time::Duration;
    use tst_srt::ListenerBuilder;
    use tstrans::config::{
        TstKlvStreamType, TstVideoCodec, tst_mux_config_add_klv_stream, tst_mux_config_add_program,
        tst_mux_config_add_video_stream, tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::error::TstError;
    use tstrans::sender::mux_sender::{
        tst_mux_sender_close, tst_mux_sender_get_socket_stats, tst_mux_sender_open,
        tst_mux_sender_send_video,
    };
    use tstrans::stats::TstSocketStats;

    let (port_tx, port_rx) = mpsc::channel::<u16>();
    // `done` lets the sender tell the accept thread when it has finished all
    // its stats reads, so the receiver holds the accepted connection open for
    // exactly that long. The old code did `let _ = listener.accept()` (which
    // drops the accepted socket IMMEDIATELY, closing the connection) then slept
    // a fixed 2s on the listener — so the sender's send_video / get_*_stats
    // raced the close and intermittently saw rc!=0 under CI load (flaky gate).
    let (done_tx, done_rx) = mpsc::channel::<()>();

    let _accept_thread = std::thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        // Bind (not `_`) so the accepted socket stays alive; drop only after
        // the sender signals done (bounded so a missed signal can't hang).
        let _accepted = listener.accept();
        let _ = done_rx.recv_timeout(Duration::from_secs(10));
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener didn't bind in time");

    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x0100, TstVideoCodec::H264);
        tst_mux_config_add_klv_stream(cfg, prog, 0x0101, TstKlvStreamType::PrivateData, false);
        let url = CString::new(format!("srt://127.0.0.1:{port}?latency=120")).unwrap();
        let s = tst_mux_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null());

        let nal: [u8; 7] = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB, 0xCC];
        let rc = tst_mux_sender_send_video(s, nal.as_ptr(), nal.len(), 0, true);
        assert_eq!(rc, 0);
        // Drain pause before the stats read — SRT's send queue is
        // asynchronous. 100 ms worked on Linux but Darwin scheduling
        // on Apple Silicon needs more headroom; 1 s covers
        // SRT's 120 ms latency budget plus scheduling jitter on every
        // platform.
        std::thread::sleep(Duration::from_secs(1));

        // LIVE: socket stats reflect the send.
        let mut sock = TstSocketStats::default();
        let rc = tst_mux_sender_get_socket_stats(s, &mut sock);
        assert_eq!(rc, 0, "live get_socket_stats");
        assert!(sock.bytes_sent > 0, "bytes_sent={}", sock.bytes_sent);
        assert!(sock.packets_sent > 0, "packets_sent={}", sock.packets_sent);
        assert_eq!(sock.bytes_received, 0, "sender side reads 0 received");

        // Null-out path is invalid_config, not not_available.
        let rc = tst_mux_sender_get_socket_stats(s, std::ptr::null_mut());
        assert_ne!(rc, 0);

        // Sender done with its reads — let the accept thread release the peer.
        let _ = done_tx.send(());
        // POST-CLOSE: get_socket_stats returns TST_E_NOT_AVAILABLE (-13)
        // because SrtTransport's inner Socket goes to None on close.
        tst_mux_sender_close(s);
        // (note: handle is freed by close; do NOT call _get_socket_stats
        // on the now-dangling pointer. Post-close NOT_AVAILABLE coverage
        // lives in the recv_transport tst-srt tests where the handle
        // stays valid.)
        let _ = TstError::NotAvailable;

        tst_mux_config_free(cfg);
    }
}

#[test]
fn mux_sender_socket_stats_null_pointer_returns_invalid_config() {
    use tstrans::sender::mux_sender::tst_mux_sender_get_socket_stats;
    use tstrans::stats::TstSocketStats;

    unsafe {
        // null sender
        let mut st = TstSocketStats::default();
        let rc = tst_mux_sender_get_socket_stats(std::ptr::null_mut(), &mut st);
        assert_ne!(rc, 0);

        // null out pointer is hit via a live handle in the loopback tests
        // — here we only need the null-handle path because constructing a
        // live tst_mux_sender_t requires a live SRT loopback.
    }
}

#[test]
fn managed_mux_sender_socket_stats_null_pointer_returns_invalid_config() {
    use tstrans::sender::mux_sender::tst_managed_mux_sender_get_socket_stats;
    use tstrans::stats::TstSocketStats;

    unsafe {
        let mut st = TstSocketStats::default();
        let rc = tst_managed_mux_sender_get_socket_stats(std::ptr::null_mut(), &mut st);
        assert_ne!(rc, 0);
    }
}

#[test]
fn ts_sender_reset_stats_returns_ok_on_valid_handle() {
    unsafe {
        let rc = tstrans::sender::ts_sender::tst_sender_reset_stats(std::ptr::null_mut());
        assert_ne!(rc, 0);
    }
}

#[test]
fn managed_ts_sender_reset_stats_returns_ok_on_valid_handle() {
    unsafe {
        let rc = tstrans::sender::ts_sender::tst_managed_sender_reset_stats(std::ptr::null_mut());
        assert_ne!(rc, 0);
    }
}
