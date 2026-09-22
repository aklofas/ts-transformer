//! `tst_sender_t` and `tst_managed_sender_t`.
//!
//! Pre-muxed TS bytes -> SRT, with sync-byte framing/recovery (RECOVER or
//! STRICT mode per `tst_sender_config_t::framing_mode`).

use crate::config::{TstReconnectPolicy, TstSenderConfig};
use crate::error::{TstError, record_shell_error, set_last_error};
use crate::handle::Handle;
use crate::sender::mux_sender::parse_c_srt_url;
use std::sync::Arc;
use tst_pipeline::{ManagedTransport, Sender, TransportCancel};
use tst_srt::SrtTransport;

// ------------------------------------------------------------------
// tst_sender_t (plain L1)
// ------------------------------------------------------------------

pub struct TstSender {
    inner: Handle<Sender<SrtTransport>>,
    cancel: Option<Arc<dyn TransportCancel + Send + Sync>>,
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
        let mut socket_cfg = tst_srt::config::SocketConfig::default();
        url.overlay.apply_to_socket(&mut socket_cfg);
        let transport = match crate::sender::connect::connect_srt(&url.host, url.port, &socket_cfg)
        {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_binding_error(e.into());
                return std::ptr::null_mut();
            }
        };
        let sender = Sender::new(transport, cfg);
        let cancel = sender.cancel_handle();
        Box::into_raw(Box::new(TstSender {
            inner: Handle::new(sender),
            cancel,
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
        if let Some(c) = &boxed.cancel {
            c.cancel();
        }
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
/// After cancel, `_send` returns `TST_E_CLOSED`. The handle must still
/// be `_close`'d to free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_cancel(p: *mut TstSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null sender pointer");
            return TstError::InvalidConfig as i32;
        };
        // Side-channel: do NOT acquire handle.inner's Mutex (a concurrent
        // send holds it). The cancel-handle Arc is accessible without locking.
        if let Some(c) = &handle.cancel {
            c.cancel();
        }
        0
    })
}

// ------------------------------------------------------------------
// tst_managed_sender_t (managed L2)
// ------------------------------------------------------------------

pub struct TstManagedSender {
    inner: Handle<Sender<ManagedTransport<SrtTransport>>>,
    cancel: Option<Arc<dyn TransportCancel + Send + Sync>>,
    /// Reconnect/gap telemetry observer, captured from the `ManagedTransport`
    /// before it moved into the shell (same capture-before-move timing as
    /// `cancel_handle()`). Read by `tst_managed_sender_get_reconnect_stats`.
    stats_handle: tst_pipeline::ManagedStatsHandle,
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
        let mut socket_cfg = tst_srt::config::SocketConfig::default();
        url.overlay.apply_to_socket(&mut socket_cfg);

        let initial = match crate::sender::connect::connect_srt(&url.host, url.port, &socket_cfg) {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_binding_error(e.into());
                return std::ptr::null_mut();
            }
        };
        let host = url.host.clone();
        let port = url.port;
        let cfg_for_reconnect = socket_cfg.clone();
        let factory = move || crate::sender::connect::connect_srt(&host, port, &cfg_for_reconnect);
        let managed = ManagedTransport::new(initial, factory, policy);
        let stats_handle = managed.stats_handle();
        let sender = Sender::new(managed, cfg);
        let cancel = sender.cancel_handle();
        Box::into_raw(Box::new(TstManagedSender {
            inner: Handle::new(sender),
            cancel,
            stats_handle,
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
    unsafe {
        crate::transport_impls::managed_get_reconnect_stats(
            &handle.inner,
            &handle.stats_handle,
            out,
        )
    }
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
        if let Some(c) = &boxed.cancel {
            c.cancel();
        }
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_sender_cancel(p: *mut TstManagedSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null sender pointer");
            return TstError::InvalidConfig as i32;
        };
        // Side-channel: do NOT acquire handle.inner's Mutex (a concurrent
        // send holds it). The cancel-handle Arc is accessible without locking.
        if let Some(c) = &handle.cancel {
            c.cancel();
        }
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
}
