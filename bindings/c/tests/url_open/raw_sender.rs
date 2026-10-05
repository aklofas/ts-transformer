//! C-ABI URL parsing tests for `tst_raw_sender_*` (plain + managed).
//! Per-sender-variant roundtrip.

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_srt::ListenerBuilder;
use tstrans::config::{tst_raw_sender_config_free, tst_raw_sender_config_new};
use tstrans::error::TstError;
use tstrans::sender::raw_sender::{
    tst_managed_raw_sender_close, tst_managed_raw_sender_get_reconnect_stats,
    tst_managed_raw_sender_open, tst_raw_sender_close, tst_raw_sender_open,
};
use tstrans::stats::TstManagedTransportStats;

use super::last_error_msg;

#[test]
fn variant_raw_sender_open_with_url() {
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
    let url = CString::new(format!("srt://127.0.0.1:{port}?streamid=raw-plain")).unwrap();

    unsafe {
        let cfg = tst_raw_sender_config_new();
        let s = tst_raw_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("raw-plain"),
            "expected stream_id 'raw-plain', got {:?}",
            observed
        );

        tst_raw_sender_close(s);
        tst_raw_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn variant_managed_raw_sender_open_with_url() {
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
    let url = CString::new(format!("srt://127.0.0.1:{port}?streamid=raw-managed")).unwrap();

    unsafe {
        let cfg = tst_raw_sender_config_new();
        let s = tst_managed_raw_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("raw-managed"),
            "expected stream_id 'raw-managed', got {:?}",
            observed
        );

        tst_managed_raw_sender_close(s);
        tst_raw_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn variant_managed_raw_sender_get_reconnect_stats() {
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
        let cfg = tst_raw_sender_config_new();
        let s = tst_managed_raw_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        // Freshly-opened, no reconnect has ever happened — every counter
        // is zero and the padding is zeroed by the fill path.
        let mut out = TstManagedTransportStats::default();
        let rc = tst_managed_raw_sender_get_reconnect_stats(s, &mut out);
        assert_eq!(rc, 0, "get_reconnect_stats failed: {}", last_error_msg());
        assert_eq!(out.reconnect_attempts, 0);
        assert_eq!(out.reconnect_successes, 0);
        assert_eq!(out.gap_len, 0);
        assert_eq!(out.gap_messages_dropped, 0);
        assert_eq!(out.gap_bytes_dropped, 0);
        assert!(!out.reconnecting);
        assert_eq!(out._pad, [0u8; 7]);

        let rc_null_out = tst_managed_raw_sender_get_reconnect_stats(s, std::ptr::null_mut());
        assert_eq!(rc_null_out, TstError::InvalidConfig as i32);

        tst_managed_raw_sender_close(s);
        tst_raw_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}
