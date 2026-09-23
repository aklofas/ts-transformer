//! `tst_demuxer_t` — standalone offline MPEG-TS demuxer utility.
//!
//! Wraps `tst_core::mpegts::demux::Demuxer`. No transport — feed raw
//! TS bytes in, pull typed `TstEvent`s out. The handle is internally
//! synchronized.
//!
//! # Closing
//!
//! The `Demuxer` owns no transport and no OS handles. Call
//! `tst_demuxer_flush` at end-of-stream to surface partial PES still
//! buffered, drain remaining events via `tst_demuxer_next_event`, then
//! release the handle via `tst_demuxer_close`.
//!
//! ## Per-language idiom
//!
//! | Language | Idiom |
//! |----------|-------|
//! | C | `tst_demuxer_feed(d, buf, n); tst_demuxer_flush(d);` then drain `tst_demuxer_next_event` until `TST_E_NOT_AVAILABLE`, then `tst_demuxer_close(d)` |

use crate::demux_config::TstDemuxConfig;
use crate::error::{TstError, record_demux_error, record_not_available, set_last_error};
use crate::event::{EventArena, TstEvent};
use crate::handle::Handle;
use alloc::boxed::Box;
use tst_core::mpegts::demux::Demuxer;

#[cfg(not(feature = "std"))]
use crate::nostd_mutex::Mutex;
#[cfg(feature = "std")]
use std::sync::Mutex;

/// Opaque handle wrapping a `tst_core::mpegts::demux::Demuxer`.
///
/// Allocated by `tst_demuxer_open` / `tst_demuxer_open_with_config`
/// and freed by `tst_demuxer_close`. The `arena` is shared between
/// the data-path lock and the caller's `TstEvent` lifetime contract
/// (§4.5 borrowed-buffer policy: pointer fields on `TstEvent` are
/// valid until the next `tst_demuxer_next_event` or `tst_demuxer_close`
/// call on this handle).
pub struct TstDemuxer {
    inner: Handle<Demuxer>,
    arena: Mutex<EventArena>,
}

/// Open a standalone offline demuxer with default configuration.
///
/// The demuxer expects raw MPEG-TS bytes fed via `tst_demuxer_feed`;
/// no transport URL is needed. Events are pulled one at a time via
/// `tst_demuxer_next_event`.
///
/// Returns a new `tst_demuxer_t *` on success, or NULL on failure with
/// last-error set.
///
/// # Safety
///
/// The returned pointer must eventually be passed to `tst_demuxer_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demuxer_open() -> *mut TstDemuxer {
    crate::panic::ffi_catch(core::ptr::null_mut(), || {
        let demuxer = Demuxer::new();
        Box::into_raw(Box::new(TstDemuxer {
            inner: Handle::new(demuxer),
            arena: Mutex::new(EventArena::new()),
        }))
    })
}

/// Open a standalone offline demuxer with an explicit configuration.
///
/// `cfg` may be NULL — if null, the default `DemuxerConfig` is used
/// (same as `tst_demuxer_open`). The config is read at open time; the
/// caller may free it immediately after this returns.
///
/// Returns a new `tst_demuxer_t *` on success, or NULL on failure with
/// last-error set.
///
/// # Safety
///
/// `cfg` must be NULL or a valid pointer to a `tst_demux_config_t`
/// allocated by `tst_demux_config_new`. The returned pointer must
/// eventually be passed to `tst_demuxer_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demuxer_open_with_config(
    cfg: *const TstDemuxConfig,
) -> *mut TstDemuxer {
    crate::panic::ffi_catch(core::ptr::null_mut(), || {
        // NULL cfg → default options, matching demux_receiver::open_with_config semantics.
        let opts = unsafe { cfg.as_ref().map(|c| c.build_options()) };
        let demuxer = match opts {
            Some(o) => Demuxer::with_config(o),
            None => Demuxer::new(),
        };
        Box::into_raw(Box::new(TstDemuxer {
            inner: Handle::new(demuxer),
            arena: Mutex::new(EventArena::new()),
        }))
    })
}

/// Feed raw MPEG-TS bytes into the demuxer.
///
/// `data` and `len` describe the byte buffer. `len == 0` is a valid
/// no-op (returns 0). If `len > 0` and `data` is NULL the call returns
/// `TST_E_INVALID_CONFIG`.
///
/// The demuxer handles alignment internally — bytes need not be
/// 188-aligned; the sync-recovery logic manages partial packets across
/// calls.
///
/// Returns 0 on success, or a negative `TST_E_*` code:
///
/// - `TST_E_INVALID_CONFIG` (-1) — `p` is null, or `data` is null with
///   non-zero `len`.
/// - `TST_E_INVALID_TS` (-3) — `DemuxError::StrictRejection`,
///   `Unrecoverable` (one 6,016-byte sync-search window held no packet
///   boundary; the scanned bytes are discarded and the next feed starts
///   a fresh window, except that a candidate sync byte the scan stopped
///   on is retained and re-validated on the next feed rather than
///   accepted outright), `MalformedPsi`, or `MalformedPes`.
/// - `TST_E_TOO_LARGE` (-6) — `DemuxError::SyncBufExhausted` (one feed
///   would push the pre-sync buffer past `sync_buf_cap`; the buffered
///   bytes and this call's bytes are dropped and the next feed starts
///   from an empty buffer).
/// - `TST_E_CLOSED` (-7) — handle already closed.
///
/// # Safety
///
/// `p` must be a valid non-null pointer obtained from
/// `tst_demuxer_open` / `tst_demuxer_open_with_config`. `data` must
/// be non-null when `len > 0`; the bytes must remain valid for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demuxer_feed(
    p: *mut TstDemuxer,
    data: *const u8,
    len: usize,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null demuxer pointer");
        return TstError::InvalidConfig as i32;
    };
    // Zero-byte feed is a documented no-op — avoid the null-ptr check.
    if len == 0 {
        return 0;
    }
    let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    handle.inner.with_inner_mut(|d| match d.feed(slice) {
        Ok(()) => 0,
        Err(e) => record_demux_error(&e),
    })
}

/// Flush partial PES still buffered at end-of-stream.
///
/// Should be called once after the last `tst_demuxer_feed`, before the
/// final drain of `tst_demuxer_next_event`. Idempotent — calling twice
/// with no intervening `feed` is safe and a no-op the second time.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` (-1) if `p` is null,
/// or `TST_E_CLOSED` (-7) if the handle was already closed.
///
/// # Safety
///
/// `p` must be a valid non-null pointer obtained from `tst_demuxer_open`
/// / `tst_demuxer_open_with_config`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demuxer_flush(p: *mut TstDemuxer) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null demuxer pointer");
        return TstError::InvalidConfig as i32;
    };
    handle.inner.with_inner_mut(|d| {
        d.flush();
        0
    })
}

/// Pull the next typed event from the demuxer's internal queue.
///
/// On `Some(event)`: converts the Rust `DemuxEvent` into `*out_event`
/// and returns 0. Pointer fields in `*out_event` borrow from this
/// handle's `EventArena` — they are valid until the next
/// `tst_demuxer_next_event` or `tst_demuxer_close` call on the same
/// handle. Copy bytes out of `*out_event` before the next call if
/// longer lifetime is needed.
///
/// **Sentinel:** when the queue is empty (no event ready), this function
/// returns `TST_E_NOT_AVAILABLE` (-13) and leaves `*out_event`
/// unmodified. This is the **normal "no event ready" signal** — not a
/// fatal error. The caller should feed more bytes and try again, or (at
/// end-of-stream) stop polling after calling `tst_demuxer_flush`.
///
/// Returns:
///
/// - `0` — success; `*out_event` is populated.
/// - `TST_E_NOT_AVAILABLE` (-13) — no event in the queue; feed more
///   bytes or stop if at end-of-stream.
/// - `TST_E_INVALID_CONFIG` (-1) — `p` or `out_event` is null.
/// - `TST_E_CLOSED` (-7) — handle already closed.
///
/// # Safety
///
/// `p` must be a valid non-null pointer obtained from `tst_demuxer_open`
/// / `tst_demuxer_open_with_config`. `out_event` must be a writable
/// `tst_event_t`; its contents are fully overwritten on `TST_OK (0)` and
/// left unspecified on any non-zero return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demuxer_next_event(
    p: *mut TstDemuxer,
    out_event: *mut TstEvent,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null demuxer pointer");
        return TstError::InvalidConfig as i32;
    };
    if out_event.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_event pointer");
        return TstError::InvalidConfig as i32;
    }
    handle.inner.with_inner_mut(|d| match d.next_event() {
        Some(ev) => {
            // Poison = a panic mid-write on another call; the arena is
            // rewritten before it is read, so recover (binding poison
            // policy: readers recover).
            let mut arena = handle.arena.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: out_event non-null per guard above.
            // event::convert writes through the pointer; pointer fields on the
            // result alias the arena Vecs (held under the Mutex for this call;
            // released before the closure returns, but Vec base pointers are
            // stable until the next convert() call clears them — §4.5 lifetime).
            unsafe {
                crate::event::convert(&mut arena, &ev, &mut *out_event);
            }
            0
        }
        None => record_not_available("no demux event ready; feed more bytes or flush first"),
    })
}

/// Close and free the demuxer handle.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demuxer_close(p: *mut TstDemuxer) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_succeeds_and_close_is_safe() {
        unsafe {
            let d = tst_demuxer_open();
            assert!(!d.is_null());
            tst_demuxer_close(d);
        }
    }

    #[test]
    fn close_null_is_safe() {
        unsafe { tst_demuxer_close(core::ptr::null_mut()) };
    }

    #[test]
    fn next_event_empty_returns_not_available() {
        unsafe {
            let d = tst_demuxer_open();
            assert!(!d.is_null());
            let mut out = TstEvent::default();
            let rc = tst_demuxer_next_event(d, &mut out);
            assert_eq!(rc, TstError::NotAvailable as i32);
            tst_demuxer_close(d);
        }
    }

    #[test]
    fn feed_empty_slice_is_ok() {
        unsafe {
            let d = tst_demuxer_open();
            assert!(!d.is_null());
            let rc = tst_demuxer_feed(d, [].as_ptr(), 0);
            assert_eq!(rc, 0, "empty feed must return 0");
            tst_demuxer_close(d);
        }
    }

    #[test]
    fn flush_on_fresh_demuxer_is_ok() {
        unsafe {
            let d = tst_demuxer_open();
            assert!(!d.is_null());
            let rc = tst_demuxer_flush(d);
            assert_eq!(rc, 0, "flush on fresh demuxer must return 0");
            tst_demuxer_close(d);
        }
    }

    #[test]
    fn open_with_null_config_uses_defaults() {
        unsafe {
            let d = tst_demuxer_open_with_config(core::ptr::null());
            assert!(!d.is_null());
            tst_demuxer_close(d);
        }
    }
}
