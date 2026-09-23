//! `TstUdpDemuxReceiver` handle type and data-path entry points.
//!
//! Open a UDP-backed `DemuxReceiver` with `tst_udp_demux_receiver_open`.
//! Pull typed `TstEvent` items with `tst_udp_demux_receiver_next_event`.
//! Free with `tst_udp_demux_receiver_close`.
//!
//! Data-path bodies are thin forwarders to generic impls in
//! `crate::transport_impls`. The `EventArena` borrowed-buffer lifetime
//! (design §4.5), `ShellErrorKind` → error-code mapping, and the
//! per-PID stats borrowed buffer are all handled generically.
//!
//! **Cancel:** the UDP transport exposes a real cancel handle since Arc 2
//! WP-D and this handle's `CHandle` slot holds it, so `_close` from ANY
//! thread cancels first and a `_next_event` parked on the 100 ms poll loop
//! returns `TST_E_CLOSED` within one tick (pinned by
//! `tests/transports/udp_close_cancels_first.rs`).
//! `tst_udp_demux_receiver_cancel` (ABI 0.22) does the same WITHOUT freeing
//! the handle — the interrupt to use when the reader's pointer must stay
//! valid.

use std::os::raw::c_char;
use std::sync::Mutex;

use tst_pipeline::DemuxReceiver;
use tst_udp::{UdpRecvTransport, UdpRecvTransportBuilder};

use crate::demux_config::TstDemuxConfig;
use crate::error::{TstError, set_last_error};
use crate::event::{EventArena, TstEvent};
use crate::handle::{CHandle, cancel_or_latch};

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a UDP-backed demux receiver.
///
/// Returned by [`tst_udp_demux_receiver_open`]. Freed with
/// [`tst_udp_demux_receiver_close`].
pub struct TstUdpDemuxReceiver {
    pub(crate) inner: CHandle<DemuxReceiver<UdpRecvTransport>>,
    /// Reusable backing storage for `tst_udp_demux_receiver_next_event` output.
    /// Allocated at open time so the data-path call never allocates on the hot path.
    /// Wrapped in Mutex for re-entrant safety within the Handle's closure.
    pub(crate) arena: Mutex<EventArena>,
    /// Per-stream stats snapshot buffer (borrowed-buffer design §4.5).
    pub(crate) stream_stats_buf: Mutex<Vec<crate::stats::TstStreamStats>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a UDP-backed `DemuxReceiver`. `demux_cfg` may be `NULL`, in
/// which case default demux options apply (lenient / CFI-tolerant mode).
/// Returns `NULL` on error.
///
/// For unicast, pass `udp://0.0.0.0:port`. For multicast, pass the group
/// address with the ffmpeg `@` prefix (`udp://@239.0.0.1:port?iface=eth0`).
///
/// # Safety
///
/// `url` is a NUL-terminated C string. `demux_cfg` may be NULL or a
/// valid `tst_demux_config_t*`. The returned handle must eventually be
/// freed with `tst_udp_demux_receiver_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_open(
    url: *const c_char,
    demux_cfg: *const TstDemuxConfig,
) -> *mut TstUdpDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::UdpConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        let builder = match UdpRecvTransportBuilder::from_url(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::UdpConfig, &format!("udp url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "udp recv build");
                return std::ptr::null_mut();
            }
        };
        let receiver = if let Some(cfg) = unsafe { demux_cfg.as_ref() } {
            DemuxReceiver::with_demux_options(transport, cfg.build_options())
        } else {
            DemuxReceiver::new(transport)
        };
        // `cancel_or_latch` resolves the shell's `Option`; since Arc 2 WP-D
        // this transport's `cancel_handle()` is `Some`, so the handle below
        // carries the REAL cancel and `_close` wakes what it can.
        let cancel = cancel_or_latch(receiver.cancel_handle());
        Box::into_raw(Box::new(TstUdpDemuxReceiver {
            inner: CHandle::new(receiver, cancel, ()),
            arena: Mutex::new(EventArena::new()),
            stream_stats_buf: Mutex::new(Vec::new()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_udp_demux_receiver_t`.
///
/// Safe to call with `NULL` (no-op).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstUdpDemuxReceiver`
/// returned by `tst_udp_demux_receiver_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_close(p: *mut TstUdpDemuxReceiver) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

/// Interrupt a `tst_udp_demux_receiver_next_event` on another thread; that call
/// returns `TST_E_CLOSED` within one 100 ms poll tick. Callable from any thread,
/// lock-free (never takes the handle's slot), idempotent.
///
/// This is the NON-FREEING cross-thread interrupt: unlike `tst_udp_demux_receiver_close`
/// it leaves the handle valid, so the owner still frees it with
/// `tst_udp_demux_receiver_close` once no other thread is using it.
///
/// The cancel is terminal: the call that observes it and every later
/// `_recv_*` / `_next_event` on this handle return `TST_E_CLOSED`.
///
/// Returns 0, or `TST_E_INVALID_CONFIG` if `p` is null.
///
/// # Safety
///
/// `p` must be NULL or a valid, not-yet-closed `*mut TstUdpDemuxReceiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_cancel(p: *mut TstUdpDemuxReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        // `CHandle::cancel` → `Owned::cancel`: fires the transport's
        // `UdpCancelHandle` (Arc 2 WP-D) without taking the slot, so it
        // answers while a data-path call is in flight.
        handle.inner.cancel();
        0
    })
}

// ---------------------------------------------------------------------------
// Data-path entry points
// ---------------------------------------------------------------------------

/// Block until one typed `TstEvent` is ready, then populate
/// `*out_event` with the converted event.
///
/// **Borrowed buffer lifetime (design §4.5):** pointer fields on
/// `*out_event` borrow from this handle's `EventArena`. They are
/// valid until the next `_next_event` / `_close` call on the same
/// handle. Callers wanting longer lifetime memcpy out before the
/// next call.
///
/// Returns:
/// - `0` on success (`*out_event` populated)
/// - `TST_E_END_OF_STREAM` (-12) on graceful peer close / EOF
/// - `TST_E_CLOSED` (-7) if the handle was `_close`'d
/// - `TST_E_TRANSPORT` (-8) on transport failure
/// - `TST_E_INVALID_TS` (-3) on a demuxer error
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstUdpDemuxReceiver`. `out_event`
/// must be a valid writable `*mut TstEvent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_next_event(
    p: *mut TstUdpDemuxReceiver,
    out_event: *mut TstEvent,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_next_event(&handle.inner, &handle.arena, out_event)
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Snapshot aggregate stats for a `tst_udp_demux_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// NOTE: per-PID counters are NOT included here — call
/// `tst_udp_demux_receiver_get_stream_stats` to retrieve them.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpDemuxReceiver` opened via
/// `tst_udp_demux_receiver_open`. `out` must point to a writable
/// `TstDemuxReceiverStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_get_stats(
    p: *mut TstUdpDemuxReceiver,
    out: *mut crate::stats::TstDemuxReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::demux_receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying UDP socket.
///
/// `out` MUST point to a writable `TstSocketStats`; the function zeros
/// the struct on failure.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_NOT_AVAILABLE` if no live stats are available, or
/// `TST_E_CLOSED` if the handle was closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpDemuxReceiver` opened via
/// `tst_udp_demux_receiver_open`. `out` must point to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_get_socket_stats(
    p: *mut TstUdpDemuxReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_socket_stats(
            &handle.inner,
            out,
            "udp demux receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Snapshot codec-specific stats for one PID on a
/// `tst_udp_demux_receiver_t`.
///
/// The returned struct is a tagged union — read `out->kind` first, then
/// the matching `out->u.<arm>` field.
///
/// # Errors
///
/// * `TST_E_INVALID_CONFIG` — `p` or `out` is null
/// * `TST_E_CLOSED` — handle was closed
/// * `TST_E_NOT_FOUND` — `pid` has never been observed on this handle
/// * `TST_E_INTERNAL` — internal panic caught at the FFI boundary
///
/// # Safety
///
/// `p` must be a valid pointer obtained from `tst_udp_demux_receiver_open`.
/// `out` must be a writable `tst_stream_codec_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_get_stream_codec_stats(
    p: *mut TstUdpDemuxReceiver,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_stream_codec_stats(
            &handle.inner,
            pid,
            out,
            &format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this udp demux receiver)"
            ),
        )
    }
}

/// Read the Unix-epoch microsecond timestamp of the last item observed
/// on `pid` into `*out_epoch_micros`. `0` when `pid` has never been
/// observed on this handle (see
/// [`tst_demux_receiver_get_stream_last_seen_micros`](crate::receiver::demux_receiver::tst_demux_receiver_get_stream_last_seen_micros)
/// for full semantics — same shape, different handle type).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpDemuxReceiver` opened via
/// `tst_udp_demux_receiver_open`. `out_epoch_micros` must point to a
/// writable `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_get_stream_last_seen_micros(
    p: *mut TstUdpDemuxReceiver,
    pid: u16,
    out_epoch_micros: *mut u64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_stream_last_seen_micros(
            &handle.inner,
            pid,
            out_epoch_micros,
        )
    }
}

/// Reset stats counters for a `tst_udp_demux_receiver_t` to zero.
/// Also invalidates the borrowed `_get_stream_stats` snapshot
/// (design §4.5).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpDemuxReceiver` opened via
/// `tst_udp_demux_receiver_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_reset_stats(
    p: *mut TstUdpDemuxReceiver,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::demux_receiver_reset_stats(&handle.inner, &handle.stream_stats_buf)
}

/// Snapshot per-PID stats for a `tst_udp_demux_receiver_t` into the
/// handle's internal buffer; return a `(*const TstStreamStats, size_t)`
/// pair borrowing that buffer.
///
/// **Borrowed buffer lifetime (design §4.5):** `*out_array` is valid
/// until the next `_get_stream_stats` / `_reset_stats` / `_close`
/// call on the same handle. Callers wanting longer lifetime memcpy
/// the array out.
///
/// Capped at `TST_STATS_MAX_STREAMS = 64` entries (ascending PID order).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on any null pointer
/// arg, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpDemuxReceiver` opened via
/// `tst_udp_demux_receiver_open`. `out_array` and `out_count` must be
/// valid non-null pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_demux_receiver_get_stream_stats(
    p: *mut TstUdpDemuxReceiver,
    out_array: *mut *const crate::stats::TstStreamStats,
    out_count: *mut libc::size_t,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_stream_stats(
            &handle.inner,
            &handle.stream_stats_buf,
            out_array,
            out_count,
        )
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_close_is_safe() {
        unsafe { tst_udp_demux_receiver_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_next_event_returns_invalid_config() {
        let mut ev = TstEvent::default();
        let rc = unsafe { tst_udp_demux_receiver_next_event(std::ptr::null_mut(), &mut ev) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_out_event_returns_invalid_config() {
        let rc = unsafe {
            tst_udp_demux_receiver_next_event(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstDemuxReceiverStats::default();
        let rc = unsafe { tst_udp_demux_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_udp_demux_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stream_last_seen_micros_returns_invalid_config() {
        let mut micros: u64 = 0;
        let rc = unsafe {
            tst_udp_demux_receiver_get_stream_last_seen_micros(
                std::ptr::null_mut(),
                0x1011,
                &mut micros,
            )
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stream_stats_returns_invalid_config() {
        let mut arr: *const crate::stats::TstStreamStats = std::ptr::null();
        let mut count: libc::size_t = 0;
        let rc = unsafe {
            tst_udp_demux_receiver_get_stream_stats(std::ptr::null_mut(), &mut arr, &mut count)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn open_with_null_url_returns_null() {
        let p = unsafe { tst_udp_demux_receiver_open(std::ptr::null(), std::ptr::null()) };
        assert!(p.is_null());
    }
}
