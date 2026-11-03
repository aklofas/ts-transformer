//! `tst_<family>_demux_receiver_get_ts_bytes_demuxed` on the rtp, udp, tcp
//! and rist families: the counter answers while another thread is parked in
//! the receive (which holds the handle's slot) and after `_cancel`, and
//! rejects a null handle or out pointer. The SRT families are covered by
//! `receiving/demux_receiver_loopback.rs` and
//! `receiving/managed_listener_cancel.rs`.

#![cfg(any(feature = "rtp", feature = "udp", feature = "tcp", feature = "rist"))]

use std::time::{Duration, Instant};

use tstrans::error::TstError;
use tstrans::event::TstEvent;

use crate::common::scaled;
use crate::common_net::parked;

/// Park `recv` on a reader thread; from this thread read the counter
/// (it must be zero and answer promptly), cancel, and read it again.
fn answers_while_parked_and_after_cancel<T: 'static>(
    h: *mut T,
    recv: impl Fn(*mut T) -> i32 + Send + 'static,
    get: unsafe extern "C" fn(*mut T, *mut u64) -> i32,
    cancel: unsafe extern "C" fn(*mut T) -> i32,
    close: unsafe extern "C" fn(*mut T),
) {
    let rc = parked(
        h,
        recv,
        |h| {
            let started = Instant::now();
            let mut n = u64::MAX;
            assert_eq!(unsafe { get(h, &mut n) }, 0);
            assert_eq!(n, 0, "nothing was sent");
            assert!(
                started.elapsed() < scaled(Duration::from_secs(5)),
                "the counter waited for the parked receive"
            );
            unsafe { cancel(h) }
        },
        |h| unsafe { close(h) },
    );
    assert_eq!(rc, TstError::Closed as i32);
    let mut n = u64::MAX;
    assert_eq!(unsafe { get(h, &mut n) }, 0);
    assert_eq!(n, 0);
    unsafe { close(h) };
}

/// A null handle and a null out pointer are both `TST_E_INVALID_CONFIG`.
fn null_arguments_are_refused<T>(h: *mut T, get: unsafe extern "C" fn(*mut T, *mut u64) -> i32) {
    let mut n = 0u64;
    assert_eq!(
        unsafe { get(std::ptr::null_mut(), &mut n) },
        TstError::InvalidConfig as i32
    );
    assert_eq!(
        unsafe { get(h, std::ptr::null_mut()) },
        TstError::InvalidConfig as i32
    );
}

#[cfg(feature = "rtp")]
#[test]
fn rtp_ts_bytes_demuxed_answers_while_parked_and_after_cancel() {
    use std::ffi::CString;
    use tstrans::rtp::demux_receiver::*;
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let h = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(!h.is_null());
    null_arguments_are_refused(h, tst_rtp_demux_receiver_get_ts_bytes_demuxed);
    answers_while_parked_and_after_cancel(
        h,
        |h| {
            let mut ev = TstEvent::default();
            unsafe { tst_rtp_demux_receiver_next_event(h, &mut ev) }
        },
        tst_rtp_demux_receiver_get_ts_bytes_demuxed,
        tst_rtp_demux_receiver_cancel,
        tst_rtp_demux_receiver_close,
    );
}

#[cfg(feature = "udp")]
#[test]
fn udp_ts_bytes_demuxed_answers_while_parked_and_after_cancel() {
    use std::ffi::CString;
    use tstrans::udp::*;
    let url = CString::new("udp://127.0.0.1:0").unwrap();
    let h = unsafe { tst_udp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(!h.is_null());
    null_arguments_are_refused(h, tst_udp_demux_receiver_get_ts_bytes_demuxed);
    answers_while_parked_and_after_cancel(
        h,
        |h| {
            let mut ev = TstEvent::default();
            unsafe { tst_udp_demux_receiver_next_event(h, &mut ev) }
        },
        tst_udp_demux_receiver_get_ts_bytes_demuxed,
        tst_udp_demux_receiver_cancel,
        tst_udp_demux_receiver_close,
    );
}

/// Ten aligned packets from a plain socket: once the shell has fed them to
/// the demuxer, the counter equals the shell's own `bytes_received`.
#[cfg(feature = "udp")]
#[test]
fn udp_ts_bytes_demuxed_equals_bytes_received() {
    use std::ffi::CString;
    use tstrans::stats::TstDemuxReceiverStats;
    use tstrans::udp::*;

    let port = crate::common_net::free_udp_port();
    let url = CString::new(format!("udp://127.0.0.1:{port}")).unwrap();
    let h = unsafe { tst_udp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(!h.is_null());

    // Null packets yield no events, so the pull parks until the cancel below.
    let reader = parked_reader(h);

    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let packet = crate::common::ts_null_packet();
    for _ in 0..10 {
        sock.send_to(&packet, ("127.0.0.1", port)).unwrap();
    }

    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    let mut n = 0u64;
    while n < 10 * 188 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            unsafe { tst_udp_demux_receiver_get_ts_bytes_demuxed(h, &mut n) },
            0
        );
    }
    unsafe { tst_udp_demux_receiver_cancel(h) };
    reader.join().expect("reader thread");

    let mut stats = TstDemuxReceiverStats::default();
    assert_eq!(
        unsafe { tst_udp_demux_receiver_get_stats(h, &mut stats) },
        0
    );
    assert_eq!(n, 10 * 188, "the counter never reached the sent bytes");
    assert_eq!(n, stats.bytes_received);
    unsafe { tst_udp_demux_receiver_close(h) };

    fn parked_reader(h: *mut TstUdpDemuxReceiver) -> std::thread::JoinHandle<()> {
        let p = crate::common_net::SendPtr(h);
        std::thread::spawn(move || {
            let p = p;
            let crate::common_net::SendPtr(h) = p;
            let mut ev = TstEvent::default();
            unsafe { tst_udp_demux_receiver_next_event(h, &mut ev) };
        })
    }
}

#[cfg(feature = "tcp")]
#[test]
fn tcp_ts_bytes_demuxed_answers_while_parked_and_after_cancel() {
    use std::ffi::CString;
    use tstrans::tcp::*;
    // A peer that accepts and stays silent keeps the receive parked.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let held = listener.accept().expect("accept");
        std::thread::sleep(scaled(Duration::from_secs(3)));
        drop(held);
    });
    let url = CString::new(format!("tcp://127.0.0.1:{port}")).unwrap();
    let h = unsafe { tst_tcp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(!h.is_null());
    null_arguments_are_refused(h, tst_tcp_demux_receiver_get_ts_bytes_demuxed);
    answers_while_parked_and_after_cancel(
        h,
        |h| {
            let mut ev = TstEvent::default();
            unsafe { tst_tcp_demux_receiver_next_event(h, &mut ev) }
        },
        tst_tcp_demux_receiver_get_ts_bytes_demuxed,
        tst_tcp_demux_receiver_cancel,
        tst_tcp_demux_receiver_close,
    );
    peer.join().expect("peer thread");
}

/// A RIST pull is one ~100 ms poll that reports `TST_E_BUFFER_FULL` while
/// nothing arrives, so the reader loops until the cancel ends it.
#[cfg(feature = "rist")]
#[test]
fn rist_ts_bytes_demuxed_answers_while_parked_and_after_cancel() {
    use std::ffi::CString;
    use tstrans::rist::*;
    // EVEN: the Simple profile puts RTCP on port + 1.
    let port: u16 = 33_130;
    let url = CString::new(format!("rist://@127.0.0.1:{port}")).unwrap();
    let h = unsafe { tst_rist_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    if h.is_null() {
        eprintln!("skip: librist could not bind {port}");
        return;
    }
    null_arguments_are_refused(h, tst_rist_demux_receiver_get_ts_bytes_demuxed);
    answers_while_parked_and_after_cancel(
        h,
        |h| loop {
            let mut ev = TstEvent::default();
            let rc = unsafe { tst_rist_demux_receiver_next_event(h, &mut ev) };
            if rc != TstError::BufferFull as i32 {
                return rc;
            }
        },
        tst_rist_demux_receiver_get_ts_bytes_demuxed,
        tst_rist_demux_receiver_cancel,
        tst_rist_demux_receiver_close,
    );
}
