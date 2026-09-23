//! `tst_sender_t` and `tst_managed_sender_t`.
//!
//! Pre-muxed TS bytes -> SRT, with sync-byte framing/recovery (RECOVER or
//! STRICT mode per `tst_sender_config_t::framing_mode`).

use crate::config::{TstReconnectPolicy, TstSenderConfig};
use crate::error::{TstError, record_binding_error, record_shell_error, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::sender::mux_sender::{parse_c_srt_url, require_caller_mode};
use tst_pipeline::binding::BindingError;
use tst_pipeline::{ManagedTransport, Sender};
use tst_srt::SrtTransport;

// ------------------------------------------------------------------
// tst_sender_t (plain L1)
// ------------------------------------------------------------------

pub struct TstSender {
    inner: CHandle<Sender<SrtTransport>>,
}

/// Open a `tst_sender_t` connected via SRT.
///
/// `srt_url` is a `srt://host:port?key=value&...` URL. Query
/// parameters apply libsrt-vocabulary options to the connection
/// (passphrase, latency, streamid, etc.). URL values override config
/// values for the same option. See
/// `docs/guides/srt.md#url-parsing` for the recognized key table.
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set in the thread-local
/// last-error for any malformed URL, unsupported key, unknown key, or
/// invalid value. The detail string from
/// `tst_get_last_error_str()` describes the specific problem.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_open(
    srt_url: *const libc::c_char,
    cfg: *const TstSenderConfig,
) -> *mut TstSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let cfg = match unsafe { cfg.as_ref() } {
            Some(c) => c.inner.clone(),
            None => tst_pipeline::SenderConfig::default(),
        };
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        // Plain senders have no listener path: refuse `?mode=listener` here,
        // before any socket (Arc 2 WP-B1 behaviour change — it used to dial
        // out as a caller regardless). `SrtUrl::connect` itself is
        // mode-agnostic by design; see `require_caller_mode`.
        if require_caller_mode(&url).is_err() {
            return std::ptr::null_mut();
        }
        // `SrtUrl::connect` owns the whole caller-mode open: overlay onto a
        // default `SocketConfig`, the sender preset underneath it, address
        // join, `Socket::connect_with`.
        let transport = match url.connect() {
            Ok(t) => t,
            Err(e) => {
                record_binding_error(BindingError::from(e));
                return std::ptr::null_mut();
            }
        };
        let sender = Sender::new(transport, cfg);
        let cancel = cancel_or_latch(sender.cancel_handle());
        Box::into_raw(Box::new(TstSender {
            inner: CHandle::new(sender, cancel, ()),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_send_ts(
    p: *mut TstSender,
    bytes: *const u8,
    len: usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(bytes, len, "bytes") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    handle.inner.with_inner_mut(|s| match s.send_ts(slice) {
        Ok(()) => 0,
        Err(e) => record_shell_error(&e),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_flush(p: *mut TstSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    handle.inner.with_inner_mut(|s| match s.flush() {
        Ok(()) => 0,
        Err(e) => record_shell_error(&e),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_get_stats(
    p: *mut TstSender,
    out: *mut crate::stats::TstSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying libsrt socket.
/// See [`tst_mux_sender_get_socket_stats`](crate::sender::mux_sender::tst_mux_sender_get_socket_stats)
/// for full semantics — same shape, different handle type.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstSender` opened via
/// `tst_sender_open` and `out` points to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_get_socket_stats(
    p: *mut TstSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::sender_get_socket_stats(
            &handle.inner,
            out,
            "ts sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_reset_stats(p: *mut TstSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::sender_reset_stats(&handle.inner)
}

/// Close and free a `tst_sender_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_close(p: *mut TstSender) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

/// Cancel a `tst_sender_t`. Unblocks a thread parked in `_send`
/// within one libsrt I/O cycle (~3-10 ms) by closing the underlying
/// libsrt socket. Safe to call from any thread. Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, the first `_send` that observes the cancel and every later
/// one return `TST_E_CLOSED` (-7). libsrt reports the closed socket as a
/// broken connection, but `SrtTransport` reads its own cancel latch
/// afterwards and reports the cancel the caller asked for (0.7.0; through
/// 0.6.x the first call reported `TST_E_TRANSPORT`). `_cancel` itself never
/// closes the shell — the handle must still be `_close`'d to free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_cancel(p: *mut TstSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null sender pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

// ------------------------------------------------------------------
// tst_managed_sender_t (managed L2)
// ------------------------------------------------------------------

pub struct TstManagedSender {
    /// Snapshot = the reconnect/gap telemetry observer captured at open
    /// (before the transport moved into the shell);
    /// `tst_managed_sender_get_reconnect_stats` reads it lock-free after a
    /// closed-check.
    inner: CHandle<Sender<ManagedTransport<SrtTransport>>, tst_pipeline::ManagedStatsHandle>,
}

/// Open a `tst_managed_sender_t` connected via SRT.
///
/// `srt_url` is a `srt://host:port?key=value&...` URL. Query
/// parameters apply libsrt-vocabulary options to the connection
/// (passphrase, latency, streamid, etc.). URL values override config
/// values for the same option. See
/// `docs/guides/srt.md#url-parsing` for the recognized key table.
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set in the thread-local
/// last-error for any malformed URL, unsupported key, unknown key, or
/// invalid value. The detail string from
/// `tst_get_last_error_str()` describes the specific problem.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_open(
    srt_url: *const libc::c_char,
    cfg: *const TstSenderConfig,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let cfg = match unsafe { cfg.as_ref() } {
            Some(c) => c.inner.clone(),
            None => tst_pipeline::SenderConfig::default(),
        };
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        // The whole managed open — initial connect, the reconnect factory
        // that re-dials the SAME url (so the overlay survives), the
        // decorator, the shell, and the observers taken before the move —
        // is one call in tst-srt (Arc 2 WP-A3). It refuses `?mode=listener`
        // itself (`SrtError::Option`), so no `require_caller_mode` here.
        let (sender, handles, stats) =
            match tst_srt::shells::managed_sender_from_url(&url, policy, cfg) {
                Ok(t) => t,
                Err(e) => {
                    record_binding_error(BindingError::from(e));
                    return std::ptr::null_mut();
                }
            };
        Box::into_raw(Box::new(TstManagedSender {
            inner: CHandle::new(sender, handles.cancel, stats),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_send_ts(
    p: *mut TstManagedSender,
    bytes: *const u8,
    len: usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(bytes, len, "bytes") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    handle.inner.with_inner_mut(|s| match s.send_ts(slice) {
        Ok(()) => 0,
        Err(e) => record_shell_error(&e),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_flush(p: *mut TstManagedSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    handle.inner.with_inner_mut(|s| match s.flush() {
        Ok(()) => 0,
        Err(e) => record_shell_error(&e),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_get_stats(
    p: *mut TstManagedSender,
    out: *mut crate::stats::TstSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_get_stats(&handle.inner, out) }
}

/// Managed sibling of [`tst_sender_get_socket_stats`]. Returns
/// `TST_E_NOT_AVAILABLE` when the reconnect loop currently has no live
/// inner socket.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstManagedSender` opened via
/// `tst_managed_sender_open` and `out` points to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_get_socket_stats(
    p: *mut TstManagedSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::sender_get_socket_stats(
            &handle.inner,
            out,
            "ts sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Snapshot reconnect/gap telemetry for a `tst_managed_sender_t` into
/// `*out`. Unlike [`tst_managed_sender_get_socket_stats`], this never
/// returns `TST_E_NOT_AVAILABLE` — the counters live on the side-channel
/// `ManagedStatsHandle`, which stays readable across reconnect gaps.
///
/// **`Blocking` mode note:** this call still contends on the shell's own
/// lock (for the closed-check), the same lock a send stuck in
/// `Blocking` mode's inline reconnect loop holds for the whole outage —
/// so it can block for the outage's duration in that mode. Polling this
/// getter without ever blocking is a `Background`-mode property (the
/// mode these stats primarily exist to observe).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_CLOSED` if the sender has been closed, or `TST_E_INTERNAL` if the
/// gap-buffer lock is poisoned (see `ManagedTransport`'s lock poisoning
/// policy — a prior panic mid-drain).
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstManagedSender` opened via
/// `tst_managed_sender_open` and `out` points to a writable
/// `tst_managed_transport_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_get_reconnect_stats(
    p: *mut TstManagedSender,
    out: *mut crate::stats::TstManagedTransportStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::managed_get_reconnect_stats(&handle.inner, out) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_reset_stats(p: *mut TstManagedSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null sender pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::sender_reset_stats(&handle.inner)
}

/// Close and free a `tst_managed_sender_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_close(p: *mut TstManagedSender) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

/// Cancel a `tst_managed_sender_t`. Same semantics as
/// `tst_sender_cancel`; reaches the currently-active inner
/// transport's cancel handle through `ManagedTransport`'s atomic
/// snapshot.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel a `_send_*` reports `TST_E_CLOSED` (-7): the managed
/// decorator latches the close, so the send never reaches libsrt (unlike
/// the plain sender, whose interrupted send surfaces as `TST_E_TRANSPORT`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_cancel(p: *mut TstManagedSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null sender pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    #[test]
    fn open_with_null_url_returns_null() {
        unsafe {
            let cfg = tst_sender_config_new();
            let p = tst_sender_open(std::ptr::null(), cfg);
            assert!(p.is_null());
            tst_sender_config_free(cfg);
        }
    }

    #[test]
    fn null_close_is_safe() {
        unsafe {
            tst_sender_close(std::ptr::null_mut());
            tst_managed_sender_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_sender_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_managed_sender_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_handle_get_reconnect_stats_returns_invalid_config() {
        let mut out = crate::stats::TstManagedTransportStats::default();
        let rc = unsafe { tst_managed_sender_get_reconnect_stats(std::ptr::null_mut(), &mut out) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    /// Arc 2 WP-B1 behaviour change: before this PR every C sender silently
    /// dialled as a caller when the URL said `?mode=listener`. `SrtUrl::connect`
    /// (and the send-side `from_url` family) now refuse it with
    /// `SrtError::Option` BEFORE any socket is touched, which A2's kind table
    /// projects to `TST_E_INVALID_CONFIG` — the code the URL parser already
    /// uses for every other rejected URL. Nothing is dialled, so port 1 is
    /// never touched and the test needs no peer.
    #[test]
    fn open_with_listener_mode_url_is_refused_before_any_socket() {
        unsafe {
            let cfg = tst_sender_config_new();
            let url = std::ffi::CString::new("srt://127.0.0.1:1?mode=listener").unwrap();
            let p = tst_sender_open(url.as_ptr(), cfg);
            assert!(p.is_null());
            assert_eq!(
                crate::error::tst_get_last_error(),
                TstError::InvalidConfig as i32
            );
            tst_sender_config_free(cfg);
        }
    }
}
