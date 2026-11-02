//! `tst_rtp_sender_flush` sends the buffered partial bundle; `_close` is
//! cancel-first and drops it.
//!
//! The shell bundles seven TS packets, and the RTP payload ceiling holds
//! six, so a full bundle leaves as a six-packet datagram plus a one-packet
//! datagram. Ten one-packet sends put one full bundle on the wire and hold
//! three packets (564 bytes) back until a flush. Each RTP datagram carries a
//! 12-byte header ahead of its TS payload.
#![cfg(feature = "rtp")]

use std::ffi::CString;
use std::net::UdpSocket;
use std::time::Duration;

use tstrans::rtp::{
    tst_rtp_sender_close, tst_rtp_sender_flush, tst_rtp_sender_open, tst_rtp_sender_send_ts,
};

use crate::common::{last_error_msg, scaled};
use crate::udp_close_cancels_first::ts_null_packet;

const RTP_HEADER: usize = 12;

fn open_pair() -> (UdpSocket, *mut tstrans::rtp::TstRtpSender) {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = rx.local_addr().unwrap().port();
    let url = CString::new(format!("rtp://127.0.0.1:{port}")).unwrap();
    let tx = unsafe { tst_rtp_sender_open(url.as_ptr()) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    (rx, tx)
}

fn send_ten(tx: *mut tstrans::rtp::TstRtpSender) {
    let pkt = ts_null_packet();
    for i in 0..10 {
        let rc = unsafe { tst_rtp_sender_send_ts(tx, pkt.as_ptr(), pkt.len()) };
        assert_eq!(rc, 0, "send {i}: {}", last_error_msg());
    }
}

#[test]
fn rtp_sender_flush_delivers_the_buffered_tail() {
    let (rx, tx) = open_pair();
    rx.set_read_timeout(Some(scaled(Duration::from_secs(2))))
        .unwrap();
    send_ten(tx);
    let rc = unsafe { tst_rtp_sender_flush(tx) };
    unsafe { tst_rtp_sender_close(tx) };
    assert_eq!(rc, 0, "flush: {}", last_error_msg());
    let mut buf = [0u8; 2048];
    let mut lengths = Vec::new();
    for _ in 0..3 {
        lengths.push(rx.recv(&mut buf).expect("datagram within the bound"));
    }
    assert_eq!(
        lengths,
        [RTP_HEADER + 1128, RTP_HEADER + 188, RTP_HEADER + 564]
    );
}

#[test]
fn rtp_sender_flush_on_an_empty_buffer_returns_zero() {
    let (_rx, tx) = open_pair();
    let rc = unsafe { tst_rtp_sender_flush(tx) };
    unsafe { tst_rtp_sender_close(tx) };
    assert_eq!(rc, 0, "flush: {}", last_error_msg());
}

/// Pins the documented contract: `_close` is cancel-first and discards the
/// buffered tail.
#[test]
fn rtp_sender_close_without_flush_drops_the_tail() {
    let (rx, tx) = open_pair();
    rx.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    send_ten(tx);
    unsafe { tst_rtp_sender_close(tx) };
    let mut buf = [0u8; 2048];
    assert_eq!(rx.recv(&mut buf).expect("six packets"), RTP_HEADER + 1128);
    assert_eq!(rx.recv(&mut buf).expect("one packet"), RTP_HEADER + 188);
    assert!(rx.recv(&mut buf).is_err(), "the tail must not arrive");
}
