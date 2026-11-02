//! `tst_rist_sender_flush` followed at once by `tst_rist_sender_close`
//! delivers the buffered tail through librist; `_close` without a flush does
//! not.
//!
//! RIST delivery is not instant: the Simple profile needs a session to
//! establish and the receive buffer holds packets for its playout delay, so
//! every wait here is a bounded poll of `tst_rist_receiver_recv_ts` (one
//! ~100 ms librist poll per call, `TST_E_BUFFER_FULL` when empty), never a
//! bare sleep.
//!
//! Every test sends a full seven-packet bundle first. The receive shell's
//! sync filter locks on four aligned TS packets before it delivers any, so a
//! stream of fewer than four packets reaches the application as nothing at
//! all; the full bundle locks it and the buffered tail of three packets that
//! follows is what the flush and close are judged on.
//!
//! Ports 33106 and 33108 are reserved for this file (EVEN — the Simple
//! profile puts RTCP on `port + 1`), disjoint from the other RIST tests'
//! ranges.
#![cfg(feature = "rist")]

use std::ffi::CString;
use std::time::{Duration, Instant};

use tstrans::error::TstError;
use tstrans::rist::{
    TstRistReceiver, TstRistSender, tst_rist_receiver_close, tst_rist_receiver_recv_ts,
    tst_rist_recv_open, tst_rist_sender_close, tst_rist_sender_flush, tst_rist_sender_open,
    tst_rist_sender_send_ts,
};

use crate::common::{last_error_msg, scaled, ts_null_packet};

fn open_pair(port: u16) -> Option<(*mut TstRistReceiver, *mut TstRistSender)> {
    let rurl = CString::new(format!("rist://@127.0.0.1:{port}")).unwrap();
    let rx = unsafe { tst_rist_recv_open(rurl.as_ptr()) };
    if rx.is_null() {
        eprintln!("skip: librist could not bind {port}");
        return None;
    }
    let surl = CString::new(format!("rist://127.0.0.1:{port}")).unwrap();
    let tx = unsafe { tst_rist_sender_open(surl.as_ptr()) };
    assert!(!tx.is_null(), "sender open: {}", last_error_msg());
    Some((rx, tx))
}

fn send_packets(tx: *mut TstRistSender, n: usize) {
    let pkt = ts_null_packet();
    for i in 0..n {
        let rc = unsafe { tst_rist_sender_send_ts(tx, pkt.as_ptr(), pkt.len()) };
        assert_eq!(rc, 0, "send {i}: {}", last_error_msg());
    }
}

/// Poll the receiver for up to `window`, returning the packets it delivered.
/// Stops early once `want` packets have arrived.
fn collect(rx: *mut TstRistReceiver, want: usize, window: Duration) -> usize {
    let mut buf = [0u8; 1316];
    let mut n = 0usize;
    let mut got = 0usize;
    let deadline = Instant::now() + window;
    while Instant::now() < deadline && got < want {
        let rc = unsafe { tst_rist_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
        if rc == 0 {
            assert_eq!(buf[0], 0x47, "a TS packet");
            got += 1;
        } else {
            assert_eq!(rc, TstError::BufferFull as i32, "{}", last_error_msg());
        }
    }
    got
}

/// Fewer packets than a bundle stay buffered in the shell; a flush sends them
/// to librist, and a `_close` right after must still let them reach the
/// receiver.
#[test]
fn rist_flush_then_close_delivers_the_buffered_tail() {
    let Some((rx, tx)) = open_pair(33106) else {
        return;
    };
    // One full bundle leaves during the sends; three packets stay buffered.
    send_packets(tx, 10);
    let rc = unsafe { tst_rist_sender_flush(tx) };
    assert_eq!(rc, 0, "flush: {}", last_error_msg());
    unsafe { tst_rist_sender_close(tx) };
    let started = Instant::now();
    let got = collect(rx, 10, scaled(Duration::from_secs(5)));
    eprintln!(
        "rist flush-then-close: {got} of 10 packets after {:?}",
        started.elapsed()
    );
    unsafe { tst_rist_receiver_close(rx) };
    assert_eq!(got, 10, "the flushed tail was lost by the close");
}

/// The negative pin: without a flush the buffered tail is discarded, so the
/// receiver gets only what the earlier full bundle sent.
#[test]
fn rist_close_without_flush_drops_the_tail() {
    let Some((rx, tx)) = open_pair(33108) else {
        return;
    };
    // One full seven-packet bundle leaves during the sends; three packets
    // stay buffered.
    send_packets(tx, 10);
    unsafe { tst_rist_sender_close(tx) };
    let got = collect(rx, 10, scaled(Duration::from_secs(5)));
    eprintln!("rist close-without-flush: {got} of 10 packets");
    unsafe { tst_rist_receiver_close(rx) };
    assert!(
        got < 10,
        "the buffered tail was delivered although close is abortive"
    );
}
