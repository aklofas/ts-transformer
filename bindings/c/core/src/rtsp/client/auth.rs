//! `tst_rtsp_client_builder_auth_*` C entry points.
//!
//! All three functions (Basic, Digest MD5, Digest SHA-256) store the same
//! username + password pair in `TstRtspClientBuilder`.  The RTSP auth
//! *scheme* is always negotiated server-side: the server sends a
//! `WWW-Authenticate` header specifying Basic or Digest (and, for Digest,
//! the algorithm); the client replies with the scheme the server demanded.
//! `RtspClientBuilder::auth` therefore takes only credentials — it does not
//! pre-select a scheme.
//!
//! The three C entry points are named for discoverability: a caller who knows
//! their camera requires Digest MD5 can find `_auth_digest_md5` in the header
//! and in search results.  At runtime the distinction has no effect on the
//! initial connection; it is the *server's* `WWW-Authenticate` that determines
//! whether Basic or Digest is actually used.

use std::os::raw::c_char;

use crate::error::{TstError, set_last_error};
use crate::handle::TstRtspClientBuilder;
use crate::panic::ffi_catch;

// ---------------------------------------------------------------------------
// Auth entry points
// ---------------------------------------------------------------------------

/// Configure HTTP Basic credentials (RFC 7617) for this builder.
///
/// Sets the username and password that will be sent in response to a
/// `WWW-Authenticate: Basic` challenge from the server.  The server
/// determines the auth scheme; this entry point is named `_auth_basic` for
/// discoverability when the caller knows their device requires Basic auth.
///
/// `user` and `pass` must be NUL-terminated UTF-8 strings.  They are copied
/// immediately; the caller's buffers do not need to outlive this call.
///
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `builder`,
/// `user`, or `pass` is NULL, or if either string is not valid UTF-8.
///
/// # Safety
///
/// - `builder` must be a non-NULL pointer from `tst_rtsp_client_builder_new`.
/// - `user` and `pass` must each be a valid, NUL-terminated C string valid
///   for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_auth_basic(
    builder: *mut TstRtspClientBuilder,
    user: *const c_char,
    pass: *const c_char,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        // SAFETY: user+pass lifetime guaranteed by caller for this call.
        if let Some((u, p)) = unsafe { crate::c_str::credentials(user, pass, "auth_basic") } {
            b.username = Some(u);
            b.password = Some(p);
        }
    });
}

/// Configure HTTP Digest MD5 credentials (RFC 7616 §3.4) for this builder.
///
/// Sets the username and password that will be sent in response to a
/// `WWW-Authenticate: Digest algorithm=MD5` challenge from the server.  The
/// server determines the auth scheme; this entry point is named
/// `_auth_digest_md5` for discoverability.
///
/// `user` and `pass` must be NUL-terminated UTF-8 strings.  They are copied
/// immediately; the caller's buffers do not need to outlive this call.
///
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `builder`,
/// `user`, or `pass` is NULL, or if either string is not valid UTF-8.
///
/// # Safety
///
/// - `builder` must be a non-NULL pointer from `tst_rtsp_client_builder_new`.
/// - `user` and `pass` must each be a valid, NUL-terminated C string valid
///   for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_auth_digest_md5(
    builder: *mut TstRtspClientBuilder,
    user: *const c_char,
    pass: *const c_char,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        // SAFETY: user+pass lifetime guaranteed by caller for this call.
        if let Some((u, p)) = unsafe { crate::c_str::credentials(user, pass, "auth_digest_md5") } {
            b.username = Some(u);
            b.password = Some(p);
        }
    });
}

/// Configure HTTP Digest SHA-256 credentials (RFC 7616 §3.4) for this
/// builder.
///
/// Sets the username and password that will be sent in response to a
/// `WWW-Authenticate: Digest algorithm=SHA-256` challenge from the server.
/// The server determines the auth scheme; this entry point is named
/// `_auth_digest_sha256` for discoverability.
///
/// `user` and `pass` must be NUL-terminated UTF-8 strings.  They are copied
/// immediately; the caller's buffers do not need to outlive this call.
///
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `builder`,
/// `user`, or `pass` is NULL, or if either string is not valid UTF-8.
///
/// # Safety
///
/// - `builder` must be a non-NULL pointer from `tst_rtsp_client_builder_new`.
/// - `user` and `pass` must each be a valid, NUL-terminated C string valid
///   for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_auth_digest_sha256(
    builder: *mut TstRtspClientBuilder,
    user: *const c_char,
    pass: *const c_char,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        // SAFETY: user+pass lifetime guaranteed by caller for this call.
        if let Some((u, p)) = unsafe { crate::c_str::credentials(user, pass, "auth_digest_sha256") }
        {
            b.username = Some(u);
            b.password = Some(p);
        }
    });
}
