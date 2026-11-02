//! A cancel is terminal for a UDP shell's buffered output: after
//! `_cancel`, a receive returns `TST_E_CLOSED` even with TS packets still
//! queued behind the syncer, a sub-bundle send returns `TST_E_CLOSED`
//! instead of buffering, and `_finish` returns `TST_E_CLOSED` instead of
//! a quiet empty flush. The binding refuses the call at the handle entry,
//! so nothing here waits on the network.
#![cfg(feature = "udp")]

use std::ffi::CString;
use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::Duration;

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::TstError;
use tstrans::udp::{
    tst_udp_mux_sender_cancel, tst_udp_mux_sender_close, tst_udp_mux_sender_finish,
    tst_udp_mux_sender_open, tst_udp_receiver_cancel, tst_udp_receiver_close,
    tst_udp_receiver_recv_ts, tst_udp_recv_open, tst_udp_sender_cancel, tst_udp_sender_close,
    tst_udp_sender_open, tst_udp_sender_send_ts,
};

use crate::common::{last_error_msg, scaled};
use crate::udp_close_cancels_first::ts_null_packet;

/// Bind :0, read the port, drop: the C receiver handle has no local-port
/// getter.
fn free_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

#[test]
fn udp_receiver_cancel_then_recv_returns_closed_not_buffered_packets() {
    let port = free_port();
    let url = CString::new(format!("udp://127.0.0.1:{port}")).unwrap();
    let rx = unsafe { tst_udp_recv_open(url.as_ptr()) };
    assert!(!rx.is_null(), "open: {}", last_error_msg());

    // 16 packets in ONE datagram: the syncer locks after four aligned
    // packets, so one recv returns a packet and the rest stay queued.
    let mut burst = Vec::with_capacity(16 * 188);
    for _ in 0..16 {
        burst.extend_from_slice(&ts_null_packet());
    }
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.send_to(&burst, ("127.0.0.1", port)).unwrap();

    // `udp://` has no `recv_timeout` key, so a lost datagram would park the
    // first receive until the test runner kills the binary. The watchdog
    // cancels the receiver instead, which fails the assertion below.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let rx_addr = rx as usize;
    let watchdog = std::thread::spawn(move || {
        if done_rx
            .recv_timeout(scaled(Duration::from_secs(5)))
            .is_err()
        {
            unsafe { tst_udp_receiver_cancel(rx_addr as *mut _) };
        }
    });
    let mut buf = vec![0u8; 188];
    let mut n = 0usize;
    let rc_first = unsafe { tst_udp_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
    let _ = done_tx.send(());
    watchdog.join().unwrap();
    assert_eq!(rc_first, 0, "first recv: {}", last_error_msg());
    assert_eq!(n, 188);

    assert_eq!(unsafe { tst_udp_receiver_cancel(rx) }, 0, "cancel rc");
    let rc = unsafe { tst_udp_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "buffered packets must not be delivered after cancel: {}",
        last_error_msg()
    );
    unsafe { tst_udp_receiver_close(rx) };
}

#[test]
fn udp_sender_cancel_then_partial_send_and_flush_return_closed() {
    let url = CString::new(format!("udp://127.0.0.1:{}", free_port())).unwrap();
    let tx = unsafe { tst_udp_sender_open(url.as_ptr()) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    assert_eq!(unsafe { tst_udp_sender_cancel(tx) }, 0, "cancel rc");
    let pkt = ts_null_packet();
    // One packet is less than a bundle: without the cancel gate it is only
    // buffered and the call returns 0.
    assert_eq!(
        unsafe { tst_udp_sender_send_ts(tx, pkt.as_ptr(), pkt.len()) },
        TstError::Closed as i32,
        "after cancel: {}",
        last_error_msg()
    );
    // The UDP sender has no `_flush` entry point yet; the matching flush
    // assertion arrives with that symbol.
    unsafe { tst_udp_sender_close(tx) };
}

#[test]
fn udp_mux_sender_cancel_then_finish_returns_closed() {
    let url = CString::new(format!("udp://127.0.0.1:{}", free_port())).unwrap();
    let (tx, cfg) = unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264 as i32);
        (tst_udp_mux_sender_open(url.as_ptr(), cfg), cfg)
    };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    assert_eq!(unsafe { tst_udp_mux_sender_cancel(tx) }, 0, "cancel rc");
    // Nothing was pushed: without the cancel gate the empty finish is 0.
    assert_eq!(
        unsafe { tst_udp_mux_sender_finish(tx) },
        TstError::Closed as i32,
        "after cancel: {}",
        last_error_msg()
    );
    unsafe { tst_udp_mux_sender_close(tx) };
}
