//! `tcps://` through the C ABI: a TLS listener and a TLS sender on
//! loopback carry TS packets end to end. Needs the `tls` feature
//! (`TST_HAS_TLS`); without it the scheme is refused at open.
#![cfg(all(feature = "tcp", feature = "tls"))]

use std::ffi::CString;

use tstrans::tcp::{
    tst_tcp_listener_accept_receiver, tst_tcp_listener_free, tst_tcp_listener_from_url,
    tst_tcp_receiver_close, tst_tcp_receiver_recv_ts, tst_tcp_sender_close, tst_tcp_sender_open,
    tst_tcp_sender_send_ts,
};

use crate::common::last_error_msg;

/// A null-PID TS packet with continuity counter `cc`.
fn ts_packet(cc: u8) -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10 | (cc & 0x0F)]);
    p
}

#[test]
fn tcps_loopback_carries_ts_packets() {
    let cert =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string(), "localhost".to_string()])
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    // The listener has no local-address getter; take a free port first.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let lurl = CString::new(format!(
        "tcps://127.0.0.1:{port}?listen=1&cert={}&key={}",
        cert_path.display(),
        key_path.display()
    ))
    .unwrap();
    let listener = unsafe { tst_tcp_listener_from_url(lurl.as_ptr()) };
    assert!(!listener.is_null(), "tcps listener: {}", last_error_msg());
    let listener_addr = listener as usize;
    let reader = std::thread::spawn(move || {
        let rx = unsafe { tst_tcp_listener_accept_receiver(listener_addr as *mut _) };
        assert!(!rx.is_null(), "accept: {}", last_error_msg());
        let mut buf = [0u8; 188];
        let mut n = 0usize;
        let rc = unsafe { tst_tcp_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
        unsafe { tst_tcp_receiver_close(rx) };
        (rc, n, buf)
    });

    let surl = CString::new(format!(
        "tcps://127.0.0.1:{port}?ca={}&connect_timeout=3",
        cert_path.display()
    ))
    .unwrap();
    let tx = unsafe { tst_tcp_sender_open(surl.as_ptr()) };
    assert!(!tx.is_null(), "tcps sender: {}", last_error_msg());
    // The receive side's sync search wants several confirming sync bytes.
    let mut bytes = Vec::new();
    for cc in 0..8 {
        bytes.extend_from_slice(&ts_packet(cc));
    }
    let rc = unsafe { tst_tcp_sender_send_ts(tx, bytes.as_ptr(), bytes.len()) };
    assert_eq!(rc, 0, "send: {}", last_error_msg());

    let (rc, n, buf) = reader.join().unwrap();
    unsafe { tst_tcp_sender_close(tx) };
    unsafe { tst_tcp_listener_free(listener) };
    assert_eq!((rc, n), (0, 188), "recv: {}", last_error_msg());
    assert_eq!(&buf[..4], &[0x47, 0x1F, 0xFF, 0x10]);
}
