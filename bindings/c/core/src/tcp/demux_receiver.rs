//! `TstTcpDemuxReceiver` handle type and data-path entry points.
//!
//! Open a TCP-backed `DemuxReceiver` with `tst_tcp_demux_receiver_open`.
//! Pull typed `TstEvent` items with `tst_tcp_demux_receiver_next_event`.
//! Free with `tst_tcp_demux_receiver_close`.
//!
//! Data-path bodies are thin forwarders to generic impls in
//! `crate::transport_impls`. The `EventArena` borrowed-buffer lifetime
//! (design §4.5), `ShellErrorKind` → error-code mapping, and the
//! per-PID stats borrowed buffer are all handled generically.
//!
//! **Single transport type:** TCP uses one `TcpTransport` that implements
//! both `Transport` and `RecvTransport`. `DemuxReceiver<TcpTransport>` uses
//! the `RecvTransport` side. Construction uses `TcpTransportBuilder::from_url`.
//!
//! **Cancel:** the Rust `TcpTransport` exposes `cancel_handle()` (since
//! PR #198) but the C ABI has no `tst_tcp_demux_receiver_cancel` entry
//! point yet (additive candidate, ABI 0.22) — `_close` simply drops the
//! handle. Without a caller-reachable cancel path there is no
//! `TST_E_CLOSED`-vs-`TST_E_END_OF_STREAM` discrimination: a graceful
//! transport close maps to `TST_E_END_OF_STREAM`.

use std::os::raw::c_char;
use std::sync::Mutex;

use tst_pipeline::DemuxReceiver;
use tst_tcp::{TcpTransport, TcpTransportBuilder};

use crate::demux_config::TstDemuxConfig;
use crate::error::{TstError, set_last_error};
use crate::event::{EventArena, TstEvent};
use crate::handle::Handle;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a TCP-backed demux receiver.
///
/// Returned by [`tst_tcp_demux_receiver_open`]. Freed with
/// [`tst_tcp_demux_receiver_close`].
pub struct TstTcpDemuxReceiver {
    pub(crate) inner: Handle<DemuxReceiver<TcpTransport>>,
    /// Reusable backing storage for `tst_tcp_demux_receiver_next_event` output.
    /// Allocated at open time so the data-path call never allocates on the hot path.
    /// Wrapped in Mutex for re-entrant safety within the Handle's closure.
    pub(crate) arena: Mutex<EventArena>,
    /// Per-stream stats snapshot buffer (borrowed-buffer design §4.5).
    pub(crate) stream_stats_buf: Mutex<Vec<crate::stats::TstStreamStats>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a TCP-backed `DemuxReceiver`. `demux_cfg` may be `NULL`, in
/// which case default demux options apply (lenient / CFI-tolerant mode).
/// Returns `NULL` on error.
///
/// The TCP connection is established synchronously (caller-side). For a
/// listener-accepted connection, use `tst_tcp_listener_accept_receiver`.
///
/// URL grammar:
/// - `tcp://host:port` — connect to a plain TCP listener
/// - `tcps://host:port` — connect with TLS (disabled if built without `tls` feature)
/// - Query params: `?nodelay=1`, `?rcvbuf=N`, `?sndbuf=N`, `?pkt_size=N`,
///   `?connect_timeout=Ns`
///
/// # Safety
///
/// `url` is a NUL-terminated C string. `demux_cfg` may be NULL or a
/// valid `tst_demux_config_t*`. The returned handle must eventually be
/// freed with `tst_tcp_demux_receiver_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_open(
    url: *const c_char,
    demux_cfg: *const TstDemuxConfig,
) -> *mut TstTcpDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::TcpConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        let builder = match TcpTransportBuilder::from_url(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::TcpConfig, &format!("tcp url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                let code = crate::error::tcp_error_to_code(&e);
                set_last_error(code, &format!("tcp connect: {e}"));
                return std::ptr::null_mut();
            }
        };
        let receiver = if let Some(cfg) = unsafe { demux_cfg.as_ref() } {
            DemuxReceiver::with_demux_options(transport, cfg.build_options())
        } else {
            DemuxReceiver::new(transport)
        };
        Box::into_raw(Box::new(TstTcpDemuxReceiver {
            inner: Handle::new(receiver),
            arena: Mutex::new(EventArena::new()),
            stream_stats_buf: Mutex::new(Vec::new()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_tcp_demux_receiver_t`.
///
/// Safe to call with `NULL` (no-op).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstTcpDemuxReceiver`
/// returned by `tst_tcp_demux_receiver_open` or
/// `tst_tcp_listener_accept_receiver` (via the DemuxReceiver variant).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_close(p: *mut TstTcpDemuxReceiver) {
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
/// `p` must be a valid non-freed `*mut TstTcpDemuxReceiver`. `out_event`
/// must be a valid writable `*mut TstEvent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_next_event(
    p: *mut TstTcpDemuxReceiver,
    out_event: *mut TstEvent,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_next_event_no_cancel(
            &handle.inner,
            &handle.arena,
            out_event,
        )
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Snapshot aggregate stats for a `tst_tcp_demux_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// NOTE: per-PID counters are NOT included here — call
/// `tst_tcp_demux_receiver_get_stream_stats` to retrieve them.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpDemuxReceiver` opened via
/// `tst_tcp_demux_receiver_open`. `out` must point to a writable
/// `TstDemuxReceiverStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_get_stats(
    p: *mut TstTcpDemuxReceiver,
    out: *mut crate::stats::TstDemuxReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::demux_receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying TCP socket.
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
/// `p` must be a valid `*mut TstTcpDemuxReceiver` opened via
/// `tst_tcp_demux_receiver_open`. `out` must point to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_get_socket_stats(
    p: *mut TstTcpDemuxReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_socket_stats(
            &handle.inner,
            out,
            "tcp demux receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Snapshot codec-specific stats for one PID on a
/// `tst_tcp_demux_receiver_t`.
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
/// `p` must be a valid pointer obtained from `tst_tcp_demux_receiver_open`.
/// `out` must be a writable `tst_stream_codec_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_get_stream_codec_stats(
    p: *mut TstTcpDemuxReceiver,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_stream_codec_stats(
            &handle.inner,
            pid,
            out,
            &format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this tcp demux receiver)"
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
/// `p` must be a valid `*mut TstTcpDemuxReceiver` opened via
/// `tst_tcp_demux_receiver_open`. `out_epoch_micros` must point to a
/// writable `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_get_stream_last_seen_micros(
    p: *mut TstTcpDemuxReceiver,
    pid: u16,
    out_epoch_micros: *mut u64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp demux receiver pointer");
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

/// Reset stats counters for a `tst_tcp_demux_receiver_t` to zero.
/// Also invalidates the borrowed `_get_stream_stats` snapshot
/// (design §4.5).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpDemuxReceiver` opened via
/// `tst_tcp_demux_receiver_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_reset_stats(
    p: *mut TstTcpDemuxReceiver,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::demux_receiver_reset_stats(&handle.inner, &handle.stream_stats_buf)
}

/// Snapshot per-PID stats for a `tst_tcp_demux_receiver_t` into the
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
/// `p` must be a valid `*mut TstTcpDemuxReceiver` opened via
/// `tst_tcp_demux_receiver_open`. `out_array` and `out_count` must be
/// valid non-null pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_demux_receiver_get_stream_stats(
    p: *mut TstTcpDemuxReceiver,
    out_array: *mut *const crate::stats::TstStreamStats,
    out_count: *mut libc::size_t,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp demux receiver pointer");
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
        unsafe { tst_tcp_demux_receiver_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_next_event_returns_invalid_config() {
        let mut ev = TstEvent::default();
        let rc = unsafe { tst_tcp_demux_receiver_next_event(std::ptr::null_mut(), &mut ev) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstDemuxReceiverStats::default();
        let rc = unsafe { tst_tcp_demux_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_tcp_demux_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stream_last_seen_micros_returns_invalid_config() {
        let mut micros: u64 = 0;
        let rc = unsafe {
            tst_tcp_demux_receiver_get_stream_last_seen_micros(
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
            tst_tcp_demux_receiver_get_stream_stats(std::ptr::null_mut(), &mut arr, &mut count)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn open_with_null_url_returns_null() {
        let p = unsafe { tst_tcp_demux_receiver_open(std::ptr::null(), std::ptr::null()) };
        assert!(p.is_null());
    }
}
