//! RTP cancel-first characterization: a `_cancel` from another thread ends a
//! PARKED receive with exactly `TST_E_CLOSED` (-7) — `RtpRecvTransport`
//! already returns `ExplicitClose` once its cancel flag is set
//! (`crates/tst-rtp/src/transport.rs`), and Arc 2's `Owned::cancel` latches
//! before it wakes the parked call, so the relabeller cannot race.
//!
//! The sibling `rtp_open_smoke.rs` covers cancel-then-recv (unparked); this
//! file covers the parked case, with a 10 s watchdog that FAILS rather than
//! hangs.
#![cfg(feature = "rtp")]

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tstrans::error::TstError;
use tstrans::event::TstEvent;
use tstrans::rtp::demux_receiver::{
    tst_rtp_demux_receiver_cancel, tst_rtp_demux_receiver_close, tst_rtp_demux_receiver_next_event,
    tst_rtp_demux_receiver_open,
};
use tstrans::rtp::receiver::{
    tst_rtp_receiver_cancel, tst_rtp_receiver_close, tst_rtp_receiver_recv_ts, tst_rtp_recv_open,
};

/// `_cancel` is documented callable from any thread; the raw pointer just
/// needs to cross the thread boundary to get there.
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}

/// Park `recv` on a reader thread, cancel from this thread, return the code
/// the reader observed. Nothing ever sends to the socket, so the receive is
/// genuinely parked.
///
/// `rescue` is what frees the reader if the cancel does NOT wake it: it owns
/// the handle and closes it, which ends the parked call so the thread can be
/// joined. Without it the watchdog would fail the test and then HANG in
/// `join()` — the same shape `receiving/cancel_first.rs` uses for its peer.
fn parked<T: 'static>(
    h: *mut T,
    recv: impl Fn(*mut T) -> i32 + Send + 'static,
    cancel: impl FnOnce(*mut T) -> i32,
    rescue: impl FnOnce(*mut T),
) -> i32 {
    let (tx, rx) = mpsc::channel::<i32>();
    let p = SendPtr(h);
    let reader = thread::spawn(move || {
        let p = p; // whole-struct capture: SendPtr is Send, its field is not
        let SendPtr(h) = p;
        tx.send(recv(h)).unwrap();
    });
    thread::sleep(Duration::from_millis(200)); // let the reader park
    assert_eq!(cancel(h), 0);
    let rc = rx.recv_timeout(Duration::from_secs(10)).ok();
    if rc.is_none() {
        // Close the handle so the parked recv returns, then drain and join
        // before asserting: a failure must be a failure, never a wedge.
        rescue(h);
        let _ = rx.recv_timeout(Duration::from_secs(5));
    }
    reader.join().expect("reader thread");
    rc.expect("_cancel did not wake the parked rtp recv within 10 s")
}

#[test]
fn rtp_receiver_parked_recv_ts_cancelled_reports_closed() {
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let h = unsafe { tst_rtp_recv_open(url.as_ptr()) };
    assert!(!h.is_null());
    let rc = parked(
        h,
        |h| {
            let mut b = [0u8; 188];
            let mut n = 0usize;
            unsafe { tst_rtp_receiver_recv_ts(h, b.as_mut_ptr(), b.len(), &mut n) }
        },
        |h| unsafe { tst_rtp_receiver_cancel(h) },
        |h| unsafe { tst_rtp_receiver_close(h) },
    );
    assert_eq!(rc, TstError::Closed as i32);
    unsafe { tst_rtp_receiver_close(h) };
}

#[test]
fn rtp_demux_receiver_parked_next_event_cancelled_reports_closed() {
    let url = CString::new("rtp://127.0.0.1:0").unwrap();
    let h = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
    assert!(!h.is_null());
    let rc = parked(
        h,
        |h| {
            let mut ev = TstEvent::default();
            unsafe { tst_rtp_demux_receiver_next_event(h, &mut ev) }
        },
        |h| unsafe { tst_rtp_demux_receiver_cancel(h) },
        |h| unsafe { tst_rtp_demux_receiver_close(h) },
    );
    assert_eq!(rc, TstError::Closed as i32);
    unsafe { tst_rtp_demux_receiver_close(h) };
}
