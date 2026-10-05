//! C-ABI URL parsing integration tests.
//!
//! Each test opens a real listener on a random local port, opens a sender
//! via the C ABI with a URL containing query params, and verifies the
//! resulting socket has the expected option values applied.
//!
//! Threading pattern: the Listener runs on a background thread (binding to
//! 127.0.0.1:0 and communicating the kernel-assigned port back via mpsc).
//! The sender is opened on the main thread so all raw C-ABI pointers
//! (*mut TstSenderConfig, *mut TstSender) never cross thread
//! boundaries.
//!
//! This file is the entry point for the folder-shaped `url_open` integration
//! test binary; the actual test bodies live in stream-type-keyed siblings
//! (see the `mod` declarations below). Shared helpers live here so each
//! sibling can pick them up via `use super::*;`.

// All sibling files import `tst_srt::ListenerBuilder` and
// `tstrans::sender::*`/`receiver::*` (all `cfg(feature = "srt")`).
#![cfg(feature = "srt")]
#![allow(unused_unsafe)]

use tstrans::error::tst_get_last_error_str;

mod demux_receiver;
mod mux_sender;
mod raw_receiver;
mod raw_sender;
mod ts_receiver;
mod ts_sender;

/// Read the C ABI's thread-local last-error string and return it as an owned
/// `String`. Returns `"<null>"` when the slot is empty so error-path tests can
/// still print something useful with `{msg}` formatting.
pub(crate) fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}
