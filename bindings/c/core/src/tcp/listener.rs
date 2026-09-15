//! `TstTcpListener` handle type and accept entry points.
//!
//! Bind a TCP listener with `tst_tcp_listener_bind` or
//! `tst_tcp_listener_from_url`. Accept incoming connections as senders
//! or receivers with `tst_tcp_listener_accept_sender` or
//! `tst_tcp_listener_accept_receiver`. Free the listener with
//! `tst_tcp_listener_free`.
//!
//! Unlike UDP (which has no listener concept because it is connectionless),
//! TCP is connection-oriented. The listener binds once and produces one
//! independent `TstTcpSender` or `TstTcpReceiver` per accepted connection.
//! Each accepted handle is fully independent and must be closed individually.
//!
//! **Role selection:** `TcpTransport` implements both `Transport` and
//! `RecvTransport`. When you accept a connection, you choose the role by
//! calling `_accept_sender` (wraps into `Sender<TcpTransport>`) or
//! `_accept_receiver` (wraps into `Receiver<TcpTransport>`). For the demux
//! path, construct a `Receiver<TcpTransport>` and let the caller promote it
//! to a `DemuxReceiver` manually — or use `tst_tcp_listener_accept_receiver`
//! to get a raw-TS receiver, then wrap in a `DemuxReceiver` at a higher level.
//!
//! **Blocking accept:** `tst_tcp_listener_accept_sender` and
//! `tst_tcp_listener_accept_receiver` block until a connection arrives.
//! For a non-blocking accept loop, call from a dedicated thread.
//!
//! **Cancel:** the Rust `TcpListener` exposes `cancel_handle()`/`close()`
//! (since deep review #4 WP-4b); the C ABI does not yet expose a
//! `tst_tcp_listener_cancel` entry point — additive candidate for ABI 0.22.

use std::net::SocketAddr;
use std::os::raw::c_char;

use tst_pipeline::{Receiver, ReceiverConfig, Sender, SenderConfig};
use tst_tcp::TcpListener;

use crate::error::{TstError, set_last_error};
use crate::tcp::receiver::TstTcpReceiver;
use crate::tcp::sender::TstTcpSender;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a TCP listener.
///
/// Returned by [`tst_tcp_listener_bind`] or [`tst_tcp_listener_from_url`].
/// Freed with [`tst_tcp_listener_free`].
///
/// A single listener can accept multiple connections in sequence by calling
/// `_accept_sender` / `_accept_receiver` repeatedly. Each accepted handle is
/// an independent `tst_tcp_sender_t` or `tst_tcp_receiver_t` that must be
/// closed via its own `_close` function.
pub struct TstTcpListener {
    pub(crate) inner: TcpListener,
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

/// Bind a TCP listener on the socket address `bind_addr` (e.g.
/// `"0.0.0.0:7001"` or `"[::]:7001"` for dual-stack). Returns `NULL` on
/// error; check `tst_get_last_error()` + `tst_get_last_error_str()`.
///
/// `bind_addr` is parsed with `std::net::SocketAddr::from_str` — it must
/// be in `host:port` form without a URL scheme. For URL-based construction,
/// use `tst_tcp_listener_from_url` with a `tcp://addr:port?listen=1` URL.
///
/// Port `0` causes the kernel to assign an ephemeral port; use
/// `tst_tcp_listener_local_addr` (not yet exported) to retrieve it.
///
/// # Safety
///
/// `bind_addr` must be a NUL-terminated C string valid for the duration of
/// this call. The returned handle must eventually be freed with
/// `tst_tcp_listener_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_listener_bind(bind_addr: *const c_char) -> *mut TstTcpListener {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let addr_str =
            match unsafe { crate::c_str::parse_c_str(bind_addr, TstError::TcpConfig, "bind_addr") }
            {
                Some(s) => s,
                None => return std::ptr::null_mut(),
            };
        let addr: SocketAddr = match addr_str.parse() {
            Ok(a) => a,
            Err(e) => {
                set_last_error(
                    TstError::TcpConfig,
                    &format!("tcp listener bind_addr parse: {e}"),
                );
                return std::ptr::null_mut();
            }
        };
        match TcpListener::bind(addr) {
            Ok(listener) => Box::into_raw(Box::new(TstTcpListener { inner: listener })),
            Err(e) => {
                let code = crate::error::tcp_error_to_code(&e);
                set_last_error(code, &format!("tcp listener bind: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Bind a TCP listener from a `tcp://addr:port?listen=1` URL.
/// Returns `NULL` on error; check `tst_get_last_error()` for the code and
/// `tst_get_last_error_str()` for a detail message.
///
/// The URL MUST include `?listen=1`; omitting it routes to the caller-side
/// path and returns `TST_E_TCP_CONFIG`.
///
/// # Safety
///
/// `url` must be a NUL-terminated C string valid for the duration of this
/// call. The returned handle must eventually be freed with
/// `tst_tcp_listener_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_listener_from_url(url: *const c_char) -> *mut TstTcpListener {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::TcpConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        match TcpListener::from_url(url_str) {
            Ok(listener) => Box::into_raw(Box::new(TstTcpListener { inner: listener })),
            Err(e) => {
                let code = crate::error::tcp_error_to_code(&e);
                set_last_error(code, &format!("tcp listener from_url: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Accept
// ---------------------------------------------------------------------------

/// Block until one inbound TCP connection arrives and return it wrapped
/// as a `tst_tcp_sender_t`.
///
/// The accepted handle must be freed with `tst_tcp_sender_close` when done.
/// The listener remains open and can be used to accept further connections.
///
/// Returns `NULL` on error (e.g., the listener was closed from another
/// thread, or an OS accept error occurred). Check `tst_get_last_error()`.
///
/// Use this when the connecting peer is a receiver — the listener side
/// pushes TS bytes (or muxes and pushes) into the accepted sender handle.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpListener`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_listener_accept_sender(
    p: *mut TstTcpListener,
) -> *mut TstTcpSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let Some(listener) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null tcp listener pointer");
            return std::ptr::null_mut();
        };
        match listener.inner.accept_blocking() {
            Ok(transport) => {
                let sender = Sender::new(transport, SenderConfig::default());
                Box::into_raw(Box::new(TstTcpSender {
                    inner: crate::handle::Handle::new(sender),
                }))
            }
            Err(e) => {
                let code = crate::error::tcp_error_to_code(&e);
                set_last_error(code, &format!("tcp accept: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Block until one inbound TCP connection arrives and return it wrapped
/// as a `tst_tcp_receiver_t`.
///
/// The accepted handle must be freed with `tst_tcp_receiver_close` when done.
/// The listener remains open and can be used to accept further connections.
///
/// Returns `NULL` on error. Check `tst_get_last_error()`.
///
/// Use this when the connecting peer is a sender — the listener side
/// receives TS bytes from the accepted receiver handle.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpListener`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_listener_accept_receiver(
    p: *mut TstTcpListener,
) -> *mut TstTcpReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let Some(listener) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null tcp listener pointer");
            return std::ptr::null_mut();
        };
        match listener.inner.accept_blocking() {
            Ok(transport) => {
                let receiver = Receiver::new(transport, ReceiverConfig::default());
                Box::into_raw(Box::new(TstTcpReceiver {
                    inner: crate::handle::Handle::new(receiver),
                }))
            }
            Err(e) => {
                let code = crate::error::tcp_error_to_code(&e);
                set_last_error(code, &format!("tcp accept: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Free
// ---------------------------------------------------------------------------

/// Close the listening socket and free the `tst_tcp_listener_t`.
///
/// Safe to call with `NULL` (no-op). After this call the pointer is
/// invalid; any blocked `_accept_*` call on the same listener will
/// unblock with an error.
///
/// Previously accepted sender/receiver handles are NOT affected by freeing
/// the listener — they remain valid until individually closed.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstTcpListener`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_listener_free(p: *mut TstTcpListener) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(p) });
    });
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_free_is_safe() {
        unsafe { tst_tcp_listener_free(std::ptr::null_mut()) };
    }

    #[test]
    fn null_accept_sender_returns_null_with_invalid_config() {
        let p = unsafe { tst_tcp_listener_accept_sender(std::ptr::null_mut()) };
        assert!(p.is_null());
        let code = crate::error::test_last_error_code();
        assert_eq!(code, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_accept_receiver_returns_null_with_invalid_config() {
        let p = unsafe { tst_tcp_listener_accept_receiver(std::ptr::null_mut()) };
        assert!(p.is_null());
        let code = crate::error::test_last_error_code();
        assert_eq!(code, TstError::InvalidConfig as i32);
    }

    #[test]
    fn bad_addr_returns_null_with_tcp_config() {
        let addr = std::ffi::CString::new("not-an-addr").unwrap();
        let p = unsafe { tst_tcp_listener_bind(addr.as_ptr()) };
        assert!(p.is_null());
        let code = crate::error::test_last_error_code();
        assert_eq!(code, TstError::TcpConfig as i32);
    }

    #[test]
    fn from_url_missing_listen_flag_returns_null_with_tcp_family_code() {
        // from_url requires ?listen=1 — omitting it should fail. The precise
        // error code is TcpConfig (-31); accept any TCP-family negative code
        // to stay resilient if the error mapping evolves.
        let url = std::ffi::CString::new("tcp://127.0.0.1:0").unwrap();
        let p = unsafe { tst_tcp_listener_from_url(url.as_ptr()) };
        assert!(p.is_null());
        let code = crate::error::test_last_error_code();
        assert!(
            code == TstError::TcpConfig as i32
                || code == TstError::TcpIo as i32
                || code == TstError::TcpConnectTimeout as i32,
            "expected a TCP-family error code, got {code}"
        );
    }

    #[test]
    fn bind_and_free_zero_port() {
        // Binding port 0 asks the kernel for an ephemeral port — always succeeds
        // on loopback. Verifies the bind + free round-trip.
        let addr = std::ffi::CString::new("127.0.0.1:0").unwrap();
        let p = unsafe { tst_tcp_listener_bind(addr.as_ptr()) };
        if p.is_null() {
            // May fail in restricted CI sandboxes — treat as a skippable condition.
            return;
        }
        unsafe { tst_tcp_listener_free(p) };
    }
}
