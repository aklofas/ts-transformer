//! `tst_demux_receiver_t` (plain) and `tst_managed_demux_receiver_t`
//! (managed) — the typed-event receiver surface.
//!
//! One `_recv_event` call = one typed `TstEvent`. Events surface
//! ProgramMap topology, decoded Samples (NAL/OBU lists or raw audio
//! frames), KLV/private Metadata records, packet/PES discontinuities,
//! and non-conformance diagnostics. Pointer fields on the returned
//! event borrow from a per-handle `EventArena` until the next
//! `_recv_event` / `_close` call (design §4.5).
//!
//! Cancellation contract: `_cancel` unblocks a thread parked in
//! `_recv_event` within ~3-10 ms (one libsrt I/O cycle). Side-channel
//! `Arc<dyn TransportCancel>` + `Arc<AtomicBool>` pattern shared with
//! `ts_receiver.rs` — `_cancel` does not acquire the handle Mutex so
//! it does not deadlock against a concurrent `_recv_event`.
//!
//! This module is split across sibling files grouped by concern:
//! - `events` — `tst_demux_receiver_recv_event` + `tst_demux_receiver_cancel`
//!   (cancel lives with events, not lifecycle, because its sole purpose
//!   is to unblock a thread parked in `_recv_event`, and the two must
//!   remain adjacent in `tstrans.h` for the byte-identical header
//!   contract — cbindgen emits all parent-module items first, then
//!   sub-modules in declaration order, so the interleaving would
//!   otherwise force `_cancel` to emit before `_recv_event`)
//! - `stats` — 5 stats accessors on the plain receiver
//! - `managed` — `TstManagedDemuxReceiver` struct + every
//!   `tst_managed_demux_receiver_*` C entry
//!
//! Sub-module declaration order = `events, stats, managed`, matching
//! the pre-split file's item sequence after the lifecycle block.
//!
//! Cross-module sibling visibility is via `pub mod` (cbindgen walks
//! them) and the re-exports below (so external callers continue to see
//! the flat `tstrans::demux_receiver::Name` path the pre-split API
//! used).

pub mod events;
pub mod stats;
pub mod managed;

pub use events::{tst_demux_receiver_cancel, tst_demux_receiver_recv_event};
pub use managed::{
    TstManagedDemuxReceiver, tst_managed_demux_receiver_cancel, tst_managed_demux_receiver_close,
    tst_managed_demux_receiver_end_reason, tst_managed_demux_receiver_get_reconnect_stats,
    tst_managed_demux_receiver_get_socket_stats, tst_managed_demux_receiver_get_stats,
    tst_managed_demux_receiver_get_stream_codec_stats,
    tst_managed_demux_receiver_get_stream_last_seen_micros,
    tst_managed_demux_receiver_get_stream_stats, tst_managed_demux_receiver_open,
    tst_managed_demux_receiver_open_listener, tst_managed_demux_receiver_open_listener_with_config,
    tst_managed_demux_receiver_open_with_config, tst_managed_demux_receiver_recv_event,
    tst_managed_demux_receiver_reset_stats,
};
pub use stats::{
    tst_demux_receiver_get_socket_stats, tst_demux_receiver_get_stats,
    tst_demux_receiver_get_stream_codec_stats, tst_demux_receiver_get_stream_last_seen_micros,
    tst_demux_receiver_get_stream_stats, tst_demux_receiver_reset_stats,
};

use crate::demux_config::TstDemuxConfig;
use crate::event::EventArena;
use crate::handle::Handle;
use crate::sender::mux_sender::{parse_c_srt_url, parse_c_srt_url_listener};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tst_pipeline::DemuxReceiver;
use tst_pipeline::TransportCancel;
use tst_srt::SrtTransport;
use tst_srt::SrtUrl;
use tst_srt::url::Mode;

// ------------------------------------------------------------------
// tst_demux_receiver_t
// ------------------------------------------------------------------

pub struct TstDemuxReceiver {
    pub(super) inner: Handle<DemuxReceiver<SrtTransport>>,
    /// Reusable per-handle backing storage for `_recv_event` output.
    /// Wrapped in Mutex because event::convert() needs &mut access
    /// from inside the Handle's accessor closure (which takes the
    /// inner Mutex internally; we add a second one here because the
    /// arena's lifetime needs to outlive any single _recv_event
    /// borrow but be re-entrant only within this handle).
    pub(super) arena: Mutex<EventArena>,
    /// Buffer for per-stream stats snapshot returned by
    /// `_get_stream_stats`. Repopulated on each call from the latest
    /// DemuxReceiver::stats().per_stream BTreeMap, capped at
    /// TST_STATS_MAX_STREAMS = 64. Borrowed-buffer lifetime per
    /// design §4.5 — valid until the next _get_stream_stats /
    /// _reset_stats / _close call.
    pub(super) stream_stats_buf: Mutex<Vec<crate::stats::TstStreamStats>>,
    pub(super) cancel: Option<Arc<dyn TransportCancel + Send + Sync>>,
    pub(super) was_cancelled: Arc<AtomicBool>,
}

/// Open a `tst_demux_receiver_t` with default demux options.
/// Accepts `srt://host:port?...` URLs; `?mode=listener` routes through
/// the listener path (equivalent to `_open_listener`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_receiver_open(
    srt_url: *const libc::c_char,
) -> *mut TstDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        if url.mode == Mode::Listener {
            return open_listener_inner(url, None);
        }
        open_caller_inner(url, None)
    })
}

/// Explicit listener-mode open with default demux options.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_receiver_open_listener(
    srt_url: *const libc::c_char,
) -> *mut TstDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = match unsafe { parse_c_srt_url_listener(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        open_listener_inner(url, None)
    })
}

/// Open a `tst_demux_receiver_t` with a caller-supplied
/// `tst_demux_config_t`. The config is cloned-from at open time;
/// the caller still owns it and must `_free` it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_receiver_open_with_config(
    srt_url: *const libc::c_char,
    cfg: *const TstDemuxConfig,
) -> *mut TstDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        let opts = unsafe { cfg.as_ref().map(|c| c.build_options()) };
        if url.mode == Mode::Listener {
            return open_listener_inner(url, opts);
        }
        open_caller_inner(url, opts)
    })
}

/// Explicit listener-mode open with a caller-supplied
/// `tst_demux_config_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_receiver_open_listener_with_config(
    srt_url: *const libc::c_char,
    cfg: *const TstDemuxConfig,
) -> *mut TstDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = match unsafe { parse_c_srt_url_listener(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        let opts = unsafe { cfg.as_ref().map(|c| c.build_options()) };
        open_listener_inner(url, opts)
    })
}

fn open_caller_inner(
    url: SrtUrl,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> *mut TstDemuxReceiver {
    let mut socket_cfg = tst_srt::config::SocketConfig::default();
    url.overlay.apply_to_socket(&mut socket_cfg);
    let transport = match crate::sender::connect::connect_srt(&url.host, url.port, &socket_cfg) {
        Ok(t) => t,
        Err(e) => {
            crate::error::record_binding_error(e.into());
            return std::ptr::null_mut();
        }
    };
    finish_open(transport, opts)
}

fn open_listener_inner(
    url: SrtUrl,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> *mut TstDemuxReceiver {
    let mut listener_cfg = tst_srt::config::ListenerConfig::default();
    url.overlay.apply_to_listener(&mut listener_cfg);
    let transport = match crate::receiver::listen::listen_srt(&url.host, url.port, &listener_cfg) {
        Ok(t) => t,
        Err(e) => {
            crate::error::record_binding_error(e.into());
            return std::ptr::null_mut();
        }
    };
    finish_open(transport, opts)
}

fn finish_open(
    transport: SrtTransport,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> *mut TstDemuxReceiver {
    let rx = match opts {
        Some(o) => DemuxReceiver::with_demux_options(transport, o),
        None => DemuxReceiver::new(transport),
    };
    let cancel = rx.cancel_handle();
    let was_cancelled = Arc::new(AtomicBool::new(false));
    Box::into_raw(Box::new(TstDemuxReceiver {
        inner: Handle::new(rx),
        arena: Mutex::new(EventArena::new()),
        stream_stats_buf: Mutex::new(Vec::new()),
        cancel,
        was_cancelled,
    }))
}

/// Close and free a `tst_demux_receiver_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_receiver_close(p: *mut TstDemuxReceiver) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.was_cancelled.store(true, Ordering::Release);
        if let Some(c) = &boxed.cancel {
            c.cancel();
        }
        boxed.inner.close();
        drop(boxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{TstError, tst_get_last_error_str};
    use crate::event::TstEvent;

    #[test]
    fn null_close_is_safe() {
        unsafe {
            tst_demux_receiver_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn null_handle_recv_event_returns_invalid_config() {
        let mut ev = TstEvent::default();
        let rc = unsafe { tst_demux_receiver_recv_event(std::ptr::null_mut(), &mut ev) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_out_recv_event_returns_invalid_config() {
        let rc =
            unsafe { tst_demux_receiver_recv_event(std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_demux_receiver_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstDemuxReceiverStats::default();
        let rc = unsafe { tst_demux_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_demux_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stream_stats_returns_invalid_config() {
        let mut arr: *const crate::stats::TstStreamStats = std::ptr::null();
        let mut count: libc::size_t = 0;
        let rc = unsafe {
            tst_demux_receiver_get_stream_stats(std::ptr::null_mut(), &mut arr, &mut count)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stream_last_seen_micros_returns_invalid_config() {
        let mut micros: u64 = 0;
        let rc = unsafe {
            tst_demux_receiver_get_stream_last_seen_micros(
                std::ptr::null_mut(),
                0x1011,
                &mut micros,
            )
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_close_is_safe() {
        unsafe {
            tst_managed_demux_receiver_close(std::ptr::null_mut());
        }
    }

    #[test]
    fn managed_null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_managed_demux_receiver_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_recv_event_returns_invalid_config() {
        let mut ev = TstEvent::default();
        let rc = unsafe { tst_managed_demux_receiver_recv_event(std::ptr::null_mut(), &mut ev) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_get_stats_returns_invalid_config() {
        let mut stats = crate::stats::TstDemuxReceiverStats::default();
        let rc = unsafe { tst_managed_demux_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_managed_demux_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_get_stream_stats_returns_invalid_config() {
        let mut arr: *const crate::stats::TstStreamStats = std::ptr::null();
        let mut count: libc::size_t = 0;
        let rc = unsafe {
            tst_managed_demux_receiver_get_stream_stats(std::ptr::null_mut(), &mut arr, &mut count)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn managed_null_get_stream_last_seen_micros_returns_invalid_config() {
        let mut micros: u64 = 0;
        let rc = unsafe {
            tst_managed_demux_receiver_get_stream_last_seen_micros(
                std::ptr::null_mut(),
                0x1011,
                &mut micros,
            )
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    /// Verify that an invalid SRT URL doesn't propagate a panic
    /// across the FFI boundary; instead returns null + sets
    /// last-error.
    #[test]
    fn open_invalid_url_returns_null_and_sets_last_error() {
        let bad = std::ffi::CString::new("not-a-url://").unwrap();
        let rx = unsafe { tst_demux_receiver_open(bad.as_ptr()) };
        assert!(rx.is_null());
        let last = unsafe { tst_get_last_error_str() };
        assert!(!last.is_null());
    }

    #[test]
    fn open_listener_invalid_url_returns_null() {
        let bad = std::ffi::CString::new("http://example.com").unwrap();
        let rx = unsafe { tst_demux_receiver_open_listener(bad.as_ptr()) };
        assert!(rx.is_null());
    }

    #[test]
    fn open_with_config_null_url_returns_null() {
        let cfg = unsafe { crate::demux_config::tst_demux_config_new() };
        let rx = unsafe { tst_demux_receiver_open_with_config(std::ptr::null(), cfg) };
        assert!(rx.is_null());
        unsafe { crate::demux_config::tst_demux_config_free(cfg) };
    }

    #[test]
    fn managed_open_with_config_null_url_returns_null() {
        let cfg = unsafe { crate::demux_config::tst_demux_config_new() };
        let rx = unsafe {
            tst_managed_demux_receiver_open_with_config(std::ptr::null(), std::ptr::null(), cfg)
        };
        assert!(rx.is_null());
        unsafe { crate::demux_config::tst_demux_config_free(cfg) };
    }
}
