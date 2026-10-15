//! Helper: parse RTP URL string to [`tst_rtp::RtpUrl`] + report parse
//! errors into the thread-local last-error slot.
//!
//! C callers pass a URL like `rtp://239.1.2.3:5000?ttl=4&iface=eth0&pkt_size=1316`.
//! `tst_rtp::RtpUrl::parse` handles all of that — this module is the
//! C-string-to-Rust-str bridge plus error mapping.

use std::os::raw::c_char;

use tst_rtp::RtpUrl;

use crate::error::{TstError, set_last_error};

/// Parse a NUL-terminated C string as an `rtp://` URL.
///
/// On error, records the appropriate last-error code + message and
/// returns `None`. On success returns the parsed [`RtpUrl`].
///
/// # Safety
///
/// `url_ptr` must be a NUL-terminated C string that is valid (non-null,
/// readable) for the entire duration of this call.
pub(crate) unsafe fn parse_url(url_ptr: *const c_char) -> Option<RtpUrl> {
    let url_str =
        unsafe { crate::c_str::parse_c_str(url_ptr, TstError::InvalidConfig, "rtp url") }?;
    match RtpUrl::parse(url_str) {
        Ok(u) => Some(u),
        Err(e) => {
            set_last_error(
                TstError::InvalidConfig,
                &format!("rtp url parse error: {e}"),
            );
            None
        }
    }
}
