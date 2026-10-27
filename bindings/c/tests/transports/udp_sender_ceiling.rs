//! A `?pkt_size=` ceiling below the bundle size splits the send into
//! whole-TS-packet datagrams that each fit the ceiling.
//!
//! Seven packets against a 1128-byte (six-packet) ceiling must arrive as
//! a 1128-byte datagram then a 188-byte one. The C sender has no flush
//! entry point: `tst_udp_sender_close` flushes the buffered tail.
#![cfg(feature = "udp")]

use std::ffi::CString;
use std::net::UdpSocket;
use std::time::Duration;

use tstrans::udp::{tst_udp_sender_close, tst_udp_sender_open, tst_udp_sender_send_ts};

fn ts_packets(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 188);
    for i in 0..n {
        out.push(0x47);
        out.extend((1..188).map(|j| ((i * 188 + j) % 251) as u8 | 1));
    }
    out
}

#[test]
fn udp_sender_splits_bundle_at_pkt_size_ceiling() {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let port = rx.local_addr().unwrap().port();

    let url = CString::new(format!("udp://127.0.0.1:{port}?pkt_size=1128")).unwrap();
    let sender = unsafe { tst_udp_sender_open(url.as_ptr()) };
    assert!(!sender.is_null(), "tst_udp_sender_open returned null");

    let input = ts_packets(7);
    let rc = unsafe { tst_udp_sender_send_ts(sender, input.as_ptr(), input.len()) };
    // Close before asserting so a failure never leaks the handle.
    unsafe { tst_udp_sender_close(sender) };
    assert_eq!(rc, 0, "tst_udp_sender_send_ts failed");

    let mut buf = [0u8; 2048];
    let mut received = Vec::new();
    let mut lengths = Vec::new();
    for _ in 0..2 {
        let n = rx.recv(&mut buf).expect("datagram within 5 s");
        lengths.push(n);
        received.extend_from_slice(&buf[..n]);
    }
    assert_eq!(lengths, [1128, 188]);
    assert_eq!(received, input);
}
