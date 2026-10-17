//! A receive-only `rtp://` key on a send URL is refused before any socket
//! opens, so this test needs no network.

#![cfg(feature = "rtp")]

use std::ffi::CString;

use tstrans::error::{TstError, tst_get_last_error};
use tstrans::rtp::tst_rtp_sender_open;

#[test]
fn sender_open_refuses_a_receive_only_key_with_the_rtp_transport_code() {
    // `pt=` is a receive-side key; the documentation address is never reached.
    let url = CString::new("rtp://192.0.2.1:5004?pt=96").unwrap();
    let h = unsafe { tst_rtp_sender_open(url.as_ptr()) };
    assert!(h.is_null(), "the refused URL must not yield a sender");
    assert_eq!(
        unsafe { tst_get_last_error() },
        TstError::RtpTransport as i32
    );
}
