//! `tst_receiver_t` (plain) and `tst_managed_receiver_t` (managed).
//!
//! One `_recv_packet` call = one 188-byte MPEG-TS packet. The underlying
//! `tst_pipeline::Receiver` runs the HUNT → VERIFY → LOCKED sync state
//! machine — bytes lost while scanning for the next aligned packet are
//! counted in `TstReceiverStats::{bytes_skipped_for_sync, resync_events}`
//! and are otherwise invisible to the caller.
//!
//! Cancellation contract: `_cancel` unblocks a thread parked in
//! `_recv_packet` within ~3-10 ms (one libsrt I/O cycle). The cancel
//! signal is delivered through the handle's `CHandle` cancel slot, which
//! is read lock-free — `_cancel` never touches the slot a concurrent
//! `_recv_packet` holds, so the two cannot deadlock.

use crate::config::TstReconnectPolicy;
use crate::error::{TstError, record_binding_error, record_recv_error, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::sender::mux_sender::{parse_c_srt_url, parse_c_srt_url_listener};
use tst_core::mpegts::common::TS_PACKET_SIZE;
use tst_pipeline::ManagedRecvTransport;
use tst_pipeline::binding::BindingError;
use tst_pipeline::{Receiver, ReceiverConfig};
use tst_srt::SrtTransport;
use tst_srt::SrtUrl;

// ------------------------------------------------------------------
// tst_receiver_t
// ------------------------------------------------------------------

pub struct TstReceiver {
    /// Slot + cancel handle + cancel latch in one. `CHandle::cancel()`
    /// reaches the libsrt socket without taking the slot, and
    /// `CHandle::is_cancelled()` is the latch the recv path reads to tell
    /// caller-initiated shutdown (`TST_E_CLOSED`) from peer FIN
    /// (`TST_E_END_OF_STREAM`) — the per-handle `was_cancelled` copy is gone.
    inner: CHandle<Receiver<SrtTransport>>,
}

/// Open a `tst_receiver_t`. Accepts `srt://host:port?...` URLs;
/// URL with `?mode=listener` is routed through the listener path
/// (equivalent to calling `tst_receiver_open_listener`).
///
/// Returns `NULL` with `TST_E_INVALID_CONFIG` set in the thread-local
/// last-error for any malformed URL, unsupported key, unknown key, or
/// invalid value. `TST_E_TRANSPORT` set on connect/bind failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_open(srt_url: *const libc::c_char) -> *mut TstReceiver {
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_open_listener(
    srt_url: *const libc::c_char,
) -> *mut TstReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = match unsafe { parse_c_srt_url_listener(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        open_inner(url)
    })
}

/// One open for both entry points: `open_plain_srt` dispatches on
/// `url.mode` (the `_listener` variants have already forced it).
fn open_inner(url: SrtUrl) -> *mut TstReceiver {
    let Ok(transport) = crate::receiver::open_plain_srt(&url) else {
        return std::ptr::null_mut();
    };
    let rx = Receiver::new(transport, ReceiverConfig::default());
    let cancel = cancel_or_latch(rx.cancel_handle());
    Box::into_raw(Box::new(TstReceiver {
        inner: CHandle::new(rx, cancel, ()),
    }))
}

/// Close and free a `tst_receiver_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_close(p: *mut TstReceiver) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        // `CHandle::close` is cancel-first: it latches `is_cancelled` and
        // trips the libsrt-level cancel BEFORE taking the slot, so any
        // concurrent recv_packet on this handle (multi-threaded misuse)
        // returns promptly and reports the caller-initiated shutdown.
        boxed.inner.close();
        drop(boxed);
    });
}

/// Block until one 188-byte MPEG-TS packet is ready, then copy it into
/// the caller's `out_packet` buffer.
///
/// `out_packet` MUST point to a buffer of at least 188 bytes (a
/// `uint8_t[188]` array on the C side). The pointer is dereferenced
/// once on success; no allocation crosses the FFI boundary.
///
/// Returns:
/// - `0` on success (188 bytes written to `out_packet`)
/// - `TST_E_END_OF_STREAM` (-12) on graceful peer close
/// - `TST_E_CLOSED` (-7) if the handle was `_close`'d, or on the call that
///   observes a cross-thread `_cancel` and every one after it
/// - `TST_E_TRANSPORT` (-8) on a transport failure other than a clean
///   peer disconnect (peer FIN surfaces as `TST_E_END_OF_STREAM`)
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments
///
/// On any non-zero return the contents of `out_packet` are unspecified.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_recv_packet(
    p: *mut TstReceiver,
    out_packet: *mut u8,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    if out_packet.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_packet pointer");
        return TstError::InvalidConfig as i32;
    }
    // The cancel latch is read on both sides of the park. The pre-park read is
    // belt-and-braces only: the latch never resets, so the post-park read
    // already covers a cancel that lands while the call is blocked. Keeping
    // both makes the intent explicit at every recv site.
    //
    // `broken_is_eos = true`: SrtTransport::recv_bytes maps a peer disconnect
    // to `TransportError::Broken` rather than `Closed` so the managed-receive
    // decorator can tell a self-initiated close from a peer-initiated break
    // and drive reconnect. At the plain C ABI boundary a Broken result on a
    // handle nobody cancelled means the peer disconnected, which the caller
    // contract documents as TST_E_END_OF_STREAM.
    let cancelled = handle.inner.is_cancelled();
    handle.inner.with_inner_mut(|rx| match rx.next_packet() {
        Ok(pkt) => {
            // SAFETY: out_packet non-null per guard above. The destination
            // is documented as a caller-provided uint8_t[188] buffer.
            unsafe { std::ptr::copy_nonoverlapping(pkt.as_ptr(), out_packet, TS_PACKET_SIZE) };
            0
        }
        Err(e) => record_recv_error(&e, cancelled || handle.inner.is_cancelled(), true),
    })
}

/// Cancel a `tst_receiver_t`. Unblocks a thread parked in
/// `_recv_packet` within one libsrt I/O cycle (~3-10 ms) by closing
/// the underlying libsrt socket. Safe to call from any thread.
/// Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, `_recv_packet` never reports `TST_E_END_OF_STREAM`: the
/// first call that observes the cancel and every later one return
/// `TST_E_CLOSED` (-7). libsrt reports the closed socket as a broken
/// connection, but `SrtTransport` reads its own cancel latch afterwards and
/// reports the cancel the caller asked for (0.7.0; through 0.6.x the first
/// call reported `TST_E_TRANSPORT`). `_cancel` itself never closes the shell
/// — the handle must still be `_close`'d to free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_cancel(p: *mut TstReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Snapshot stats for a `tst_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_get_stats(
    p: *mut TstReceiver,
    out: *mut crate::stats::TstReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying libsrt socket.
/// See [`tst_mux_sender_get_socket_stats`](crate::sender::mux_sender::tst_mux_sender_get_socket_stats)
/// for full semantics — same shape, different handle type.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstReceiver` opened via
/// `tst_receiver_open` and `out` points to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_get_socket_stats(
    p: *mut TstReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::receiver_get_socket_stats(
            &handle.inner,
            out,
            "ts receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_receiver_t` to zero. Does not
/// affect transport state or the syncer state machine.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_receiver_reset_stats(p: *mut TstReceiver) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::receiver_reset_stats(&handle.inner)
}

// ------------------------------------------------------------------
// tst_managed_receiver_t
// ------------------------------------------------------------------

pub struct TstManagedReceiver {
    /// No snapshot: this family exposes neither `_end_reason` nor
    /// `_get_reconnect_stats` (those are demux-only), so the only observer
    /// it keeps is the cancel handle `CHandle` already owns.
    inner: CHandle<Receiver<ManagedRecvTransport<SrtTransport>>>,
}

/// Open a `tst_managed_receiver_t`. URL-driven mode dispatch
/// matches `tst_receiver_open` semantics: `?mode=listener` routes
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
pub unsafe extern "C" fn tst_managed_receiver_open(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedReceiver {
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_receiver_open_listener(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedReceiver {
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

/// One managed open for both entry points. The whole composition — initial
/// open (dispatched on `url.mode`), the re-open factory that re-dials or
/// RE-ACCEPTS the same URL, the shared `FactoryCancel` slot that makes
/// every re-accept wakeable, the decorator, the shell, and the observers
/// taken before the move — lives in tst-srt (Arc 2 WP-A3). The FIRST accept
/// runs through that same slot, but nothing can fire it before this call
/// returns (DEBT-16, deferred in Arc 2).
fn managed_open_inner(
    url: SrtUrl,
    policy: tst_pipeline::ReconnectPolicy,
) -> *mut TstManagedReceiver {
    let (rx, handles) = match tst_srt::shells::managed_receiver_from_url(&url, policy) {
        Ok(t) => t,
        Err(e) => {
            record_binding_error(BindingError::from(e));
            return std::ptr::null_mut();
        }
    };
    Box::into_raw(Box::new(TstManagedReceiver {
        inner: CHandle::new(rx, handles.cancel, ()),
    }))
}

/// Block until one 188-byte MPEG-TS packet is ready. Semantics match
/// `tst_receiver_recv_packet`; on transport failure the managed
/// inner reconnects transparently before returning an error only once
/// the retry budget is exhausted.
///
/// # Asymmetry with `tst_receiver_recv_packet`
///
/// The plain `tst_receiver_recv_packet` maps `TransportError::Broken`
/// on a non-cancelled handle to `TST_E_END_OF_STREAM` (peer disconnect at
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
pub unsafe extern "C" fn tst_managed_receiver_recv_packet(
    p: *mut TstManagedReceiver,
    out_packet: *mut u8,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    if out_packet.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_packet pointer");
        return TstError::InvalidConfig as i32;
    }
    // `broken_is_eos = false`: the decorator already retried on Broken, so a
    // Broken reaching here is a hard transport failure, not a peer close
    // (see the asymmetry note above).
    let cancelled = handle.inner.is_cancelled();
    handle.inner.with_inner_mut(|rx| match rx.next_packet() {
        Ok(pkt) => {
            // SAFETY: out_packet non-null per guard above.
            unsafe { std::ptr::copy_nonoverlapping(pkt.as_ptr(), out_packet, TS_PACKET_SIZE) };
            0
        }
        Err(e) => record_recv_error(&e, cancelled || handle.inner.is_cancelled(), false),
    })
}

/// Cancel a `tst_managed_receiver_t`. Unblocks a thread parked in
/// `_recv_packet` within one libsrt I/O cycle (~3-10 ms). Safe from
/// any thread. Idempotent. After cancel, `_recv_packet` returns
/// `TST_E_CLOSED`. The handle must still be `_close`'d to free memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_receiver_cancel(p: *mut TstManagedReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Close and free a `tst_managed_receiver_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_receiver_close(p: *mut TstManagedReceiver) {
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

/// Snapshot stats for a `tst_managed_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_receiver_get_stats(
    p: *mut TstManagedReceiver,
    out: *mut crate::stats::TstReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_get_stats(&handle.inner, out) }
}

/// Managed sibling of [`tst_receiver_get_socket_stats`]. Returns
/// `TST_E_NOT_AVAILABLE` when the reconnect loop currently has no live
/// inner socket.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstManagedReceiver` opened via
/// `tst_managed_receiver_open` and `out` points to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_receiver_get_socket_stats(
    p: *mut TstManagedReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::receiver_get_socket_stats(
            &handle.inner,
            out,
            "ts receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_managed_receiver_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_receiver_reset_stats(
    p: *mut TstManagedReceiver,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::receiver_reset_stats(&handle.inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_close_is_safe() {
        unsafe {
            tst_receiver_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn null_handle_recv_packet_returns_invalid_config() {
        let mut buf = [0u8; 188];
        let rc = unsafe { tst_receiver_recv_packet(std::ptr::null_mut(), buf.as_mut_ptr()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_buf_recv_packet_returns_invalid_config() {
        // Both null pointers trip the p guard first; reaching the buf
        // guard requires a non-null handle, which needs in-process
        // loopback testing — deferred to ts_receiver_loopback.rs.
        let rc = unsafe { tst_receiver_recv_packet(std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_receiver_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstReceiverStats::default();
        let rc = unsafe { tst_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_close_is_safe() {
        unsafe {
            tst_managed_receiver_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn managed_null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_managed_receiver_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_recv_packet_returns_invalid_config() {
        let mut buf = [0u8; 188];
        let rc =
            unsafe { tst_managed_receiver_recv_packet(std::ptr::null_mut(), buf.as_mut_ptr()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstReceiverStats::default();
        let rc = unsafe { tst_managed_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_managed_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
