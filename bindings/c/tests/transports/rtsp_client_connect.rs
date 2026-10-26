//! `rtsps://` / `rtsp://` client connect through the C ABI: a connect to a
//! misbehaving peer returns NULL with the right error code instead of
//! parking the caller.
#![cfg(feature = "rtp")]

#[cfg(feature = "tls")]
use std::io::Read;
#[cfg(feature = "tls")]
use std::net::TcpListener;
#[cfg(feature = "tls")]
use std::thread::JoinHandle;

/// A peer that accepts one connection, reads exactly one TLS record (the
/// ClientHello: 5-byte header + its length), then closes with a FIN. Reading
/// the whole record matters: closing with unread bytes would send RST, which
/// already took the error path.
#[cfg(feature = "tls")]
fn fin_after_client_hello_peer() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut header = [0u8; 5];
        s.read_exact(&mut header).unwrap();
        let len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).unwrap();
    });
    (port, peer)
}

/// The peer's FIN mid-handshake surfaces as `TST_E_RTSP_TLS`; before the
/// fix the handshake loop polled the closed socket forever.
#[cfg(feature = "tls")]
#[test]
fn rtsps_connect_to_a_peer_closing_mid_handshake_is_rtsp_tls() {
    use std::ffi::CString;
    use std::sync::mpsc;
    use std::time::Duration;
    use tstrans::error::{TstError, tst_get_last_error};
    use tstrans::{
        tst_rtsp_client_builder_connect, tst_rtsp_client_builder_keepalive,
        tst_rtsp_client_builder_new,
    };

    let (port, peer) = fin_after_client_hello_peer();
    let url = CString::new(format!("rtsps://127.0.0.1:{port}/x")).unwrap();
    let (tx, rx) = mpsc::channel();
    // The last-error slot is thread-local: read it on the connecting thread.
    std::thread::spawn(move || unsafe {
        let b = tst_rtsp_client_builder_new(url.as_ptr());
        assert!(!b.is_null(), "{}", crate::common::last_error_msg());
        tst_rtsp_client_builder_keepalive(b, false);
        let session = tst_rtsp_client_builder_connect(b);
        let _ = tx.send((
            session.is_null(),
            tst_get_last_error(),
            crate::common::last_error_msg(),
        ));
    });
    let (is_null, code, msg) = rx
        .recv_timeout(crate::common::scaled(Duration::from_secs(10)))
        .expect("connect did not return: the handshake loop is spinning on EOF");
    assert!(is_null, "connect to a closing peer returned a session");
    assert_eq!(code, TstError::RtspTls as i32, "{msg}");
    peer.join().unwrap();
}
