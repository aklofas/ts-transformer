//! Helpers shared by every tst-c integration-test binary. Each binary root
//! declares `#[allow(dead_code)] mod common;` because per-file feature
//! gates decide which helpers a given build actually calls.

use tstrans::error::tst_get_last_error_str;

/// Read the C ABI's thread-local last-error string as an owned `String`;
/// `"<null>"` when the slot is empty, so a failing assertion still prints
/// something useful.
pub(crate) fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}
