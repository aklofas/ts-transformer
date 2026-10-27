//! Shared C-string -> `&str` bridges for URL and config-string arguments.
//!
//! Every C string argument needs the same checks: reject null, validate
//! UTF-8, and record an error code with a message naming the argument.
//! [`parse_c_str`] says `"{name} pointer is null"` (the transport-family URL
//! arguments and `rtp/url.rs`); [`c_str_arg`] says `"{name} is null"` (the
//! RTSP builder, mount and publish arguments). [`credentials`] is the
//! user/pass pair the RTSP client and server auth setters share.

use crate::error::{TstError, set_last_error};
use std::ffi::CStr;
use std::os::raw::c_char;

/// The shared body: `null_msg` is recorded for a null pointer,
/// `"{name} is not valid UTF-8"` for bad bytes.
///
/// # Safety
/// `ptr` is NULL or a NUL-terminated C string valid for the call.
unsafe fn checked<'a>(
    ptr: *const c_char,
    code: TstError,
    name: &str,
    null_msg: impl FnOnce() -> String,
) -> Option<&'a str> {
    if ptr.is_null() {
        set_last_error(code, &null_msg());
        return None;
    }
    // SAFETY: caller-asserted NUL-termination + validity.
    match unsafe { CStr::from_ptr(ptr) }.to_str() {
        Ok(s) => Some(s),
        Err(_) => {
            set_last_error(code, &format!("{name} is not valid UTF-8"));
            None
        }
    }
}

/// Parse a NUL-terminated C string `ptr` and return a borrowed `&str` if it
/// is valid UTF-8 and non-null. On error, records `code` with
/// `"{name} pointer is null"` or `"{name} is not valid UTF-8"` and returns
/// `None`.
///
/// # Safety
/// `ptr` must be a NUL-terminated C string, valid for the duration of the
/// call.
pub(crate) unsafe fn parse_c_str<'a>(
    ptr: *const c_char,
    code: TstError,
    name: &str,
) -> Option<&'a str> {
    unsafe { checked(ptr, code, name, || format!("{name} pointer is null")) }
}

/// [`parse_c_str`] with the RTSP entry points' null wording,
/// `"{name} is null"`.
///
/// # Safety
/// As [`parse_c_str`].
#[cfg(feature = "rtp")]
pub(crate) unsafe fn c_str_arg<'a>(
    ptr: *const c_char,
    code: TstError,
    name: &str,
) -> Option<&'a str> {
    unsafe { checked(ptr, code, name, || format!("{name} is null")) }
}

/// Copy the (user, pass) pair an auth setter receives. Records
/// `TST_E_INVALID_CONFIG` with `"{context}: user or pass pointer is null"`
/// or `"{context}: username is not valid UTF-8"` /
/// `"{context}: password is not valid UTF-8"` and returns `None`.
///
/// # Safety
/// `user` and `pass` are each NULL or a NUL-terminated C string valid for
/// the call.
#[cfg(feature = "rtp")]
pub(crate) unsafe fn credentials(
    user: *const c_char,
    pass: *const c_char,
    context: &str,
) -> Option<(String, secrecy::SecretString)> {
    if user.is_null() || pass.is_null() {
        set_last_error(
            TstError::InvalidConfig,
            &format!("{context}: user or pass pointer is null"),
        );
        return None;
    }
    let u = unsafe {
        checked(
            user,
            TstError::InvalidConfig,
            &format!("{context}: username"),
            String::new,
        )
    }?;
    let p = unsafe {
        checked(
            pass,
            TstError::InvalidConfig,
            &format!("{context}: password"),
            String::new,
        )
    }?;
    // `to_owned` allocates the exact length, so the boxed str is not
    // reallocated and no unwiped copy is left behind.
    Some((u.to_owned(), secrecy::SecretString::from(p.to_owned())))
}
