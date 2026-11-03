//! RTP cancel-first characterization: a `_cancel` from another thread ends a
//! PARKED receive with exactly `TST_E_CLOSED` (-7) — `RtpRecvTransport`
//! already returns `ExplicitClose` once its cancel flag is set
//! (`crates/tst-rtp/src/transport/recv.rs`), and `Owned::cancel` latches
//! before it wakes the parked call, so the relabeller cannot race.
//!
//! The sibling `rtp_open_smoke.rs` covers cancel-then-recv (unparked); this
//! file covers the parked case, with a 10 s watchdog that FAILS rather than
//! hangs.
#![cfg(feature = "rtp")]

use crate::common::parked;
use std::ffi::CString;
use tstrans::error::TstError;
use tstrans::event::TstEvent;
use tstrans::rtp::demux_receiver::{
    tst_rtp_demux_receiver_cancel, tst_rtp_demux_receiver_close, tst_rtp_demux_receiver_next_event,
    tst_rtp_demux_receiver_open,
};
use tstrans::rtp::receiver::{
    tst_rtp_receiver_cancel, tst_rtp_receiver_close, tst_rtp_receiver_recv_ts, tst_rtp_recv_open,
};

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
