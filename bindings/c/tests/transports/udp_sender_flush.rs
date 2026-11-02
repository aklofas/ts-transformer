//! `tst_udp_sender_flush` sends the buffered partial bundle; `_close` is
//! cancel-first and drops it.
//!
//! The shell sends a datagram per seven TS packets. Ten one-packet sends
//! put one 1316-byte bundle on the wire and hold three packets (564
//! bytes) back until a flush.
#![cfg(feature = "udp")]

use std::ffi::CString;
use std::net::UdpSocket;
use std::time::Duration;

use tstrans::udp::{
    tst_udp_sender_close, tst_udp_sender_flush, tst_udp_sender_open, tst_udp_sender_send_ts,
};

use crate::common::{last_error_msg, scaled};
use crate::udp_close_cancels_first::ts_null_packet;

fn open_pair() -> (UdpSocket, *mut tstrans::udp::TstUdpSender) {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = rx.local_addr().unwrap().port();
    let url = CString::new(format!("udp://127.0.0.1:{port}")).unwrap();
    let tx = unsafe { tst_udp_sender_open(url.as_ptr()) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    (rx, tx)
}

fn send_ten(tx: *mut tstrans::udp::TstUdpSender) {
    let pkt = ts_null_packet();
    for i in 0..10 {
        let rc = unsafe { tst_udp_sender_send_ts(tx, pkt.as_ptr(), pkt.len()) };
        assert_eq!(rc, 0, "send {i}: {}", last_error_msg());
    }
}

#[test]
fn udp_sender_flush_delivers_the_buffered_tail() {
    let (rx, tx) = open_pair();
    rx.set_read_timeout(Some(scaled(Duration::from_secs(2))))
        .unwrap();
    send_ten(tx);
    let rc = unsafe { tst_udp_sender_flush(tx) };
    unsafe { tst_udp_sender_close(tx) };
    assert_eq!(rc, 0, "flush: {}", last_error_msg());
    let mut buf = [0u8; 2048];
    let mut lengths = Vec::new();
    for _ in 0..2 {
        lengths.push(rx.recv(&mut buf).expect("datagram within the bound"));
    }
    assert_eq!(lengths, [1316, 564]);
}

#[test]
fn udp_sender_flush_on_an_empty_buffer_returns_zero() {
    let (_rx, tx) = open_pair();
    let rc = unsafe { tst_udp_sender_flush(tx) };
    unsafe { tst_udp_sender_close(tx) };
    assert_eq!(rc, 0, "flush: {}", last_error_msg());
}

/// Pins the documented contract: `_close` is cancel-first and discards
/// the buffered tail.
#[test]
fn udp_sender_close_without_flush_drops_the_tail() {
    let (rx, tx) = open_pair();
    rx.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    send_ten(tx);
    unsafe { tst_udp_sender_close(tx) };
    let mut buf = [0u8; 2048];
    let n = rx.recv(&mut buf).expect("the full bundle");
    assert_eq!(n, 1316);
    assert!(rx.recv(&mut buf).is_err(), "the tail must not arrive");
}
