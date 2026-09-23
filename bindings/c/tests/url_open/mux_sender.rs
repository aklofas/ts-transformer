//! C-ABI URL parsing tests for `tst_mux_sender_*` (plain + managed).
//! Per spec §8.3 second paragraph (per-sender-variant roundtrip).

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_srt::ListenerBuilder;
use tstrans::config::{
    TstKlvStreamType, TstVideoCodec, tst_mux_config_add_klv_stream, tst_mux_config_add_program,
    tst_mux_config_add_video_stream, tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::TstError;
use tstrans::sender::mux_sender::{
    tst_managed_mux_sender_close, tst_managed_mux_sender_finish,
    tst_managed_mux_sender_get_reconnect_stats, tst_managed_mux_sender_open,
    tst_managed_mux_sender_send_video, tst_mux_sender_close, tst_mux_sender_finish,
    tst_mux_sender_open, tst_mux_sender_send_video,
};
use tstrans::stats::TstManagedTransportStats;

use super::last_error_msg;

#[test]
fn variant_mux_sender_open_with_url() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (sid_tx, sid_rx) = mpsc::channel::<Option<String>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (accepted, _peer) = listener.accept().expect("accept");
        sid_tx
            .send(accepted.stream_id().map(str::to_string))
            .expect("send stream_id");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?streamid=mux-plain")).unwrap();

    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
        let s = tst_mux_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("mux-plain"),
            "expected stream_id 'mux-plain', got {:?}",
            observed
        );

        tst_mux_sender_close(s);
        tst_mux_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn variant_managed_mux_sender_open_with_url() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (sid_tx, sid_rx) = mpsc::channel::<Option<String>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (accepted, _peer) = listener.accept().expect("accept");
        sid_tx
            .send(accepted.stream_id().map(str::to_string))
            .expect("send stream_id");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?streamid=mux-managed")).unwrap();

    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
        let s = tst_managed_mux_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("mux-managed"),
            "expected stream_id 'mux-managed', got {:?}",
            observed
        );

        tst_managed_mux_sender_close(s);
        tst_mux_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn variant_managed_mux_sender_get_reconnect_stats() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
        let s = tst_managed_mux_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        // Freshly-opened, no reconnect has ever happened — every counter
        // is zero and the padding is zeroed by the fill path.
        let mut out = TstManagedTransportStats::default();
        let rc = tst_managed_mux_sender_get_reconnect_stats(s, &mut out);
        assert_eq!(rc, 0, "get_reconnect_stats failed: {}", last_error_msg());
        assert_eq!(out.reconnect_attempts, 0);
        assert_eq!(out.reconnect_successes, 0);
        assert_eq!(out.gap_len, 0);
        assert_eq!(out.gap_messages_dropped, 0);
        assert_eq!(out.gap_bytes_dropped, 0);
        assert!(!out.reconnecting);
        assert_eq!(out._pad, [0u8; 7]);

        let rc_null_out = tst_managed_mux_sender_get_reconnect_stats(s, std::ptr::null_mut());
        assert_eq!(rc_null_out, TstError::InvalidConfig as i32);

        tst_managed_mux_sender_close(s);
        tst_mux_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ---------------------------------------------------------------------------
// `_finish` — Arc 2 R3 (DEBT-14 "ship now" cell), ABI 0.22
// ---------------------------------------------------------------------------

/// Holds a loopback SRT peer alive for the whole test. Dropping it releases
/// the accepted socket and joins the peer thread, so a test that panics
/// still tears the listener down.
struct HeldPeer {
    done_tx: mpsc::Sender<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for HeldPeer {
    fn drop(&mut self) {
        let _ = self.done_tx.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Bind a loopback listener on an ephemeral port, accept one connection on a
/// background thread and HOLD the accepted socket until the returned guard
/// drops. Unlike the open-smoke bodies above (which accept and immediately
/// drop), `_finish` needs the peer alive so the drain has somewhere to go.
///
/// Every wait is bounded: the peer thread gives up after its accept timeout
/// and after a 30 s hold timeout, so a failing test fails rather than wedges.
fn hold_loopback_peer(streamid: &str) -> (String, HeldPeer) {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let join = thread::spawn(move || {
        let Ok(mut listener) = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
        else {
            return;
        };
        let Ok(addr) = listener.local_addr() else {
            return;
        };
        if port_tx.send(addr.port()).is_err() {
            return;
        }
        let accepted = listener.accept();
        // Hold the accepted socket until the test is done with it (bounded,
        // so a panicking test never leaves this thread parked forever).
        let _ = done_rx.recv_timeout(Duration::from_secs(30));
        drop(accepted);
    });
    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    (
        format!("srt://127.0.0.1:{port}?streamid={streamid}"),
        HeldPeer {
            done_tx,
            join: Some(join),
        },
    )
}

/// Build the one-video-stream config every `_finish` test below uses.
///
/// # Safety
///
/// The returned pointer must be freed with `tst_mux_config_free`.
unsafe fn finish_test_config() -> *mut tstrans::config::TstMuxConfig {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        cfg
    }
}

/// Arc 2 R3: `_finish` drains and closes; a second `_finish` is 0; the
/// handle still frees with `_close`.
#[test]
fn mux_sender_finish_drains_then_reports_zero_and_is_idempotent() {
    let (url_str, _peer) = hold_loopback_peer("mux-finish");
    let url = CString::new(url_str).unwrap();

    unsafe {
        let cfg = finish_test_config();
        let tx = tst_mux_sender_open(url.as_ptr(), cfg);
        tst_mux_config_free(cfg);
        assert!(!tx.is_null(), "open failed: {}", last_error_msg());

        let nal = [0u8, 0, 0, 1, 0x65, 0xBB];
        let rc = tst_mux_sender_send_video(tx, nal.as_ptr(), nal.len(), 0, true);
        assert_eq!(rc, 0, "send rc={rc}: {}", last_error_msg());

        let rc = tst_mux_sender_finish(tx);
        assert_eq!(rc, 0, "finish rc={rc}: {}", last_error_msg());
        assert_eq!(tst_mux_sender_finish(tx), 0, "second finish must be 0");

        // After finish the sender is closed: a send reports TST_E_CLOSED.
        let rc = tst_mux_sender_send_video(tx, nal.as_ptr(), nal.len(), 3000, true);
        assert_eq!(rc, TstError::Closed as i32, "post-finish send rc={rc}");

        tst_mux_sender_close(tx);
    }
}

#[test]
fn mux_sender_finish_null_is_invalid_config() {
    assert_eq!(
        unsafe { tst_mux_sender_finish(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
}

#[test]
fn managed_mux_sender_finish_is_zero_then_closed() {
    let (url_str, _peer) = hold_loopback_peer("managed-finish");
    let url = CString::new(url_str).unwrap();

    unsafe {
        let cfg = finish_test_config();
        let tx = tst_managed_mux_sender_open(url.as_ptr(), cfg, std::ptr::null());
        tst_mux_config_free(cfg);
        assert!(!tx.is_null(), "open failed: {}", last_error_msg());

        let nal = [0u8, 0, 0, 1, 0x65, 0xBB];
        let rc = tst_managed_mux_sender_send_video(tx, nal.as_ptr(), nal.len(), 0, true);
        assert_eq!(rc, 0, "send rc={rc}: {}", last_error_msg());

        let rc = tst_managed_mux_sender_finish(tx);
        assert_eq!(rc, 0, "finish rc={rc}: {}", last_error_msg());
        assert_eq!(tst_managed_mux_sender_finish(tx), 0);

        let rc = tst_managed_mux_sender_send_video(tx, nal.as_ptr(), nal.len(), 3000, true);
        assert_eq!(rc, TstError::Closed as i32, "post-finish send rc={rc}");

        tst_managed_mux_sender_close(tx);
    }
}

#[test]
fn managed_mux_sender_finish_null_is_invalid_config() {
    assert_eq!(
        unsafe { tst_managed_mux_sender_finish(std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
}
