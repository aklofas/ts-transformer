//! `tst_raw_receiver_t` (plain) and `tst_managed_raw_receiver_t` (managed).
//!
//! One `_recv` call = one inbound SRT message into the caller's buffer.
//! No MPEG-TS framing or sync recovery — that's `tst_receiver_t`.
//!
//! Cancellation contract: `_cancel` unblocks a thread parked in `_recv`
//! within ~3-10 ms (one libsrt I/O cycle). The cancel signal is
//! delivered through the handle's `CHandle` cancel slot, which is read
//! lock-free — `_cancel` never touches the slot a concurrent `_recv`
//! holds, so the two cannot deadlock.

use crate::config::TstReconnectPolicy;
use crate::error::{TstError, record_binding_error, record_recv_error, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::sender::mux_sender::{parse_c_srt_url, parse_c_srt_url_listener};
use tst_pipeline::ManagedRecvTransport;
use tst_pipeline::binding::BindingError;
use tst_pipeline::{RawReceiver, RawReceiverConfig};
use tst_srt::SrtTransport;
use tst_srt::SrtUrl;

// ------------------------------------------------------------------
// tst_raw_receiver_t
// ------------------------------------------------------------------

pub struct TstRawReceiver {
    /// Slot + cancel handle + cancel latch in one; see `TstReceiver`.
    inner: CHandle<RawReceiver<SrtTransport>>,
}

/// Open a `tst_raw_receiver_t`. Accepts `srt://host:port?...` URLs;
/// URL with `?mode=listener` is routed through the listener path
/// (equivalent to calling `tst_raw_receiver_open_listener`).
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set in the thread-local
/// last-error for any malformed URL, unsupported key, unknown key, or
/// invalid value. `TST_E_TRANSPORT` set on connect/bind failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_open(
    srt_url: *const libc::c_char,
) -> *mut TstRawReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        open_inner(url)
    })
}

/// Explicit listener-mode open. Forces listener mode regardless of any
/// `?mode=` URL value — the `_listener` suffix is authoritative. URLs
/// with `?mode=caller` are accepted and silently overridden.
///
/// Empty-host URLs like `srt://:7000` are accepted directly; the parser's
/// requirement for an explicit `?mode=listener` does not apply here because
/// the entry-point name is already the authoritative listener signal.
///
/// (Simplification of the design spec §4.2, which originally proposed
/// rejecting explicit `mode=caller` with `TST_E_INVALID_USAGE`. The
/// simpler rule is more forgiving and matches what most C consumers
/// expect from a `_listener`-suffixed entry point. The stricter check
/// can land later if a consumer asks.)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_open_listener(
    srt_url: *const libc::c_char,
) -> *mut TstRawReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = match unsafe { parse_c_srt_url_listener(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        open_inner(url)
    })
}

/// One open for both entry points: `open_plain_srt` dispatches on
/// `url.mode` (the `_listener` variant has already forced it).
fn open_inner(url: SrtUrl) -> *mut TstRawReceiver {
    let Ok(transport) = crate::receiver::open_plain_srt(&url) else {
        return std::ptr::null_mut();
    };
    let rx = RawReceiver::new(transport, RawReceiverConfig::default());
    let cancel = cancel_or_latch(rx.cancel_handle());
    Box::into_raw(Box::new(TstRawReceiver {
        inner: CHandle::new(rx, cancel, ()),
    }))
}

/// Close and free a `tst_raw_receiver_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_close(p: *mut TstRawReceiver) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        // `CHandle::close` is cancel-first: a concurrent recv on this handle
        // (multi-threaded misuse) returns promptly and reports the
        // caller-initiated shutdown, not end-of-stream.
        boxed.inner.close();
        drop(boxed);
    });
}

/// Cancel a `tst_raw_receiver_t`. Unblocks a thread parked in `_recv`
/// within one libsrt I/O cycle (~3-10 ms) by closing the underlying
/// libsrt socket. Safe to call from any thread. Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, `_recv` never reports `TST_E_END_OF_STREAM`: the first call
/// that observes the cancel and every later one return `TST_E_CLOSED` (-7).
/// libsrt reports the closed socket as a broken connection, but
/// `SrtTransport` reads its own cancel latch afterwards and reports the
/// cancel the caller asked for (0.7.0; through 0.6.x the first call reported
/// `TST_E_TRANSPORT`). `_cancel` itself never closes the shell — the handle
/// must still be `_close`'d to free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_cancel(p: *mut TstRawReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Snapshot stats for a `tst_raw_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_get_stats(
    p: *mut TstRawReceiver,
    out: *mut crate::stats::TstRawRecvStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::raw_receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying libsrt socket.
/// See [`tst_mux_sender_get_socket_stats`](crate::sender::mux_sender::tst_mux_sender_get_socket_stats)
/// for full semantics — same shape, different handle type.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstRawReceiver` opened via
/// `tst_raw_receiver_open` and `out` points to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_get_socket_stats(
    p: *mut TstRawReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::raw_receiver_get_socket_stats(
            &handle.inner,
            out,
            "raw receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_raw_receiver_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_reset_stats(p: *mut TstRawReceiver) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::raw_receiver_reset_stats(&handle.inner)
}

/// Block until one message arrives. Copies up to `len` bytes into `buf`
/// and writes the actual length to `*out_len`.
///
/// Returns:
/// - 0 on success (`*out_len` set to bytes received; ≤ `len`)
/// - `TST_E_END_OF_STREAM` (-12) on graceful peer close
/// - `TST_E_CLOSED` (-7) if the handle was `_cancel`'d or `_close`'d
/// - `TST_E_TRANSPORT` (-8) on a transport failure other than a clean
///   peer disconnect (peer FIN surfaces as `TST_E_END_OF_STREAM`; see
///   the `TransportError::Broken` arm in this function for details)
/// - `TST_E_TOO_LARGE` (-6) if the inbound message exceeds `len`
///   (`*out_len` is left unmodified)
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_receiver_recv(
    p: *mut TstRawReceiver,
    buf: *mut u8,
    len: usize,
    out_len: *mut usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    if buf.is_null() && len > 0 {
        set_last_error(TstError::InvalidConfig, "null buf with non-zero len");
        return TstError::InvalidConfig as i32;
    }
    if out_len.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_len pointer");
        return TstError::InvalidConfig as i32;
    }
    // See `tst_receiver_recv_packet` for why the latch is read on both sides
    // of the park and why `broken_is_eos` is `true` on a plain shell.
    let cancelled = handle.inner.is_cancelled();
    handle.inner.with_inner_mut(|rx| match rx.recv_one() {
        Ok(v) => {
            if v.len() > len {
                set_last_error(
                    TstError::TooLarge,
                    &format!("message {} bytes exceeds buf cap {}", v.len(), len),
                );
                return TstError::TooLarge as i32;
            }
            if !v.is_empty() {
                unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len()) };
            }
            // SAFETY: out_len non-null per guard above.
            unsafe { *out_len = v.len() };
            0
        }
        Err(e) => record_recv_error(&e, cancelled || handle.inner.is_cancelled(), true),
    })
}

// ------------------------------------------------------------------
// tst_managed_raw_receiver_t
// ------------------------------------------------------------------

pub struct TstManagedRawReceiver {
    /// No snapshot: this family exposes neither `_end_reason` nor
    /// `_get_reconnect_stats`; see `TstManagedReceiver`.
    inner: CHandle<RawReceiver<ManagedRecvTransport<SrtTransport>>>,
}

/// Open a `tst_managed_raw_receiver_t`. URL-driven mode dispatch
/// matches `tst_raw_receiver_open` semantics: `?mode=listener` routes
/// to the listener path, otherwise caller mode.
///
/// On transport failure the managed wrapper automatically reconnects
/// (or re-binds for listener mode) according to `policy`. Pass `NULL`
/// for `policy` to use the default reconnect policy.
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set in the thread-local
/// last-error for any malformed URL. `TST_E_TRANSPORT` set on the
/// initial connect/bind failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_open(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedRawReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        managed_open_inner(url, policy)
    })
}

/// Explicit listener-mode open for the managed receiver. Forces
/// listener mode regardless of any `?mode=` URL value — the
/// `_listener` suffix is authoritative. On peer disconnect the managed
/// wrapper re-binds a fresh listener socket and accepts the next
/// incoming connection. Note: the re-bind + re-accept may block
/// significantly between attempts depending on the reconnect policy;
/// `_cancel` wakes both the backoff wait and a re-accept parked with no
/// peer, so a cancel lands promptly in that window too.
///
/// Empty-host URLs like `srt://:7000` are accepted directly; the parser's
/// requirement for an explicit `?mode=listener` does not apply here because
/// the entry-point name is already the authoritative listener signal.
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set for malformed URLs
/// or `TST_E_TRANSPORT` on the initial bind/accept failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_open_listener(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedRawReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url_listener(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        managed_open_inner(url, policy)
    })
}

/// One managed open for both entry points; see `TstManagedReceiver`'s
/// `managed_open_inner`. `managed_raw_receiver_from_url` applies
/// `RawReceiverConfig::default()` internally — what this family passed by
/// hand before Arc 2.
fn managed_open_inner(
    url: SrtUrl,
    policy: tst_pipeline::ReconnectPolicy,
) -> *mut TstManagedRawReceiver {
    let (rx, handles) = match tst_srt::shells::managed_raw_receiver_from_url(&url, policy) {
        Ok(t) => t,
        Err(e) => {
            record_binding_error(BindingError::from(e));
            return std::ptr::null_mut();
        }
    };
    Box::into_raw(Box::new(TstManagedRawReceiver {
        inner: CHandle::new(rx, handles.cancel, ()),
    }))
}

/// Block until one message arrives. Semantics match `tst_raw_receiver_recv`;
/// on transport failure the managed inner reconnects transparently before
/// returning an error only once the retry budget is exhausted.
///
/// # Asymmetry with `tst_raw_receiver_recv`
///
/// The plain `tst_raw_receiver_recv` maps `TransportError::Broken` on
/// a non-cancelled handle to `TST_E_END_OF_STREAM` (peer disconnect at
/// the bare-transport layer is semantically end-of-stream). The managed
/// version does NOT apply that mapping: `ManagedRecvTransport`
/// already retries internally on Broken, so a Broken that reaches this
/// function is the inner-cancel-mutex-poisoned path — a hard transport
/// failure (`TST_E_TRANSPORT`), not an end-of-stream.
///
/// **Reconnect budget exhaustion is a different path, and it is NOT
/// `TST_E_TRANSPORT`:** when the configured reconnect policy gives up,
/// `ManagedRecvTransport` latches its inner transport `Closed` (the same
/// state a clean peer close leaves it in — SRT cannot tell "peer hung up"
/// from "peer never came back" at this layer), and this function returns
/// `TST_E_END_OF_STREAM`, exactly like a clean end of stream. This family
/// has no end-reason getter (that's demux-only), so a clean teardown and
/// a give-up-after-retries are indistinguishable from this return value
/// alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_recv(
    p: *mut TstManagedRawReceiver,
    buf: *mut u8,
    len: usize,
    out_len: *mut usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    if buf.is_null() && len > 0 {
        set_last_error(TstError::InvalidConfig, "null buf with non-zero len");
        return TstError::InvalidConfig as i32;
    }
    if out_len.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_len pointer");
        return TstError::InvalidConfig as i32;
    }
    // `broken_is_eos = false` — see the asymmetry note above.
    let cancelled = handle.inner.is_cancelled();
    handle.inner.with_inner_mut(|rx| match rx.recv_one() {
        Ok(v) => {
            if v.len() > len {
                set_last_error(
                    TstError::TooLarge,
                    &format!("message {} bytes exceeds buf cap {}", v.len(), len),
                );
                return TstError::TooLarge as i32;
            }
            if !v.is_empty() {
                unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len()) };
            }
            // SAFETY: out_len non-null per guard above.
            unsafe { *out_len = v.len() };
            0
        }
        Err(e) => record_recv_error(&e, cancelled || handle.inner.is_cancelled(), false),
    })
}

/// Cancel a `tst_managed_raw_receiver_t`. Unblocks a thread parked in
/// `_recv` within one libsrt I/O cycle (~3-10 ms). Safe from any thread.
/// Idempotent. After cancel, `_recv` returns `TST_E_CLOSED`. The handle
/// must still be `_close`'d to free memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_cancel(
    p: *mut TstManagedRawReceiver,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Close and free a `tst_managed_raw_receiver_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_close(p: *mut TstManagedRawReceiver) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        // `CHandle::close` is cancel-first (see `tst_receiver_close`).
        boxed.inner.close();
        drop(boxed);
    });
}

/// Snapshot stats for a `tst_managed_raw_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_get_stats(
    p: *mut TstManagedRawReceiver,
    out: *mut crate::stats::TstRawRecvStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::raw_receiver_get_stats(&handle.inner, out) }
}

/// Reset stats counters for a `tst_managed_raw_receiver_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
/// Managed sibling of [`tst_raw_receiver_get_socket_stats`]. Returns
/// `TST_E_NOT_AVAILABLE` when the reconnect loop currently has no live
/// inner socket.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstManagedRawReceiver` opened
/// via `tst_managed_raw_receiver_open` and `out` points to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_get_socket_stats(
    p: *mut TstManagedRawReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::raw_receiver_get_socket_stats(
            &handle.inner,
            out,
            "raw receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_raw_receiver_reset_stats(
    p: *mut TstManagedRawReceiver,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::raw_receiver_reset_stats(&handle.inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_null_close_is_safe() {
        unsafe {
            tst_managed_raw_receiver_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn managed_null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_managed_raw_receiver_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_recv_returns_invalid_config() {
        let mut buf = [0u8; 16];
        let mut got: usize = 0;
        let rc = unsafe {
            tst_managed_raw_receiver_recv(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut got,
            )
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstRawRecvStats::default();
        let rc = unsafe { tst_managed_raw_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_managed_raw_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_close_is_safe() {
        unsafe {
            tst_raw_receiver_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn null_pointer_recv_returns_invalid_config() {
        let mut buf = [0u8; 16];
        let mut got: usize = 0;
        let rc = unsafe {
            tst_raw_receiver_recv(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut got)
        };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_handle_and_null_buf_both_return_invalid_config() {
        // Both null pointers trip the p guard first (line 161-164), not the buf
        // guard (line 165-168). Reaching the buf guard requires a non-null
        // handle; in-process loopback testing lives in the integration suite.
        let mut got: usize = 0;
        let rc = unsafe {
            tst_raw_receiver_recv(std::ptr::null_mut(), std::ptr::null_mut(), 16, &mut got)
        };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_raw_receiver_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstRawRecvStats::default();
        let rc = unsafe { tst_raw_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_raw_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
