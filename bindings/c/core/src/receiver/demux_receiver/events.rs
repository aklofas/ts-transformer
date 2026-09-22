//! `tst_demux_receiver_recv_event` + `tst_demux_receiver_cancel` —
//! the event-marshalling step and its side-channel cancel.
//!
//! Receives one typed `TstEvent` per call by walking the demuxer pull
//! loop and converting Rust `DemuxEvent` items into C-shaped events
//! against the per-handle `EventArena` (design §4.5 borrowed-buffer
//! lifetime). `_cancel` lives alongside `_recv_event` (rather than with
//! the `_open` / `_close` lifecycle in `mod.rs`) because its sole
//! purpose is to unblock a thread parked in `_recv_event`, and the two
//! must remain adjacent in `tstrans.h` for the byte-identical header
//! contract (cbindgen emits all parent-module items first, then
//! sub-modules in declaration order). Sibling-managed variant lives in
//! `managed.rs`.

use super::TstDemuxReceiver;
use crate::error::{TstError, record_recv_closed, record_recv_error, set_last_error};
use crate::event::TstEvent;

/// Block until one typed `TstEvent` is ready, then populate
/// `*out_event` with the converted event.
///
/// **Borrowed buffer lifetime (design §4.5):** pointer fields on
/// `*out_event` borrow from this handle's `EventArena`. They are
/// valid until the next `_recv_event` / `_close` call on the same
/// handle. Callers wanting longer lifetime memcpy out before the
/// next call.
///
/// Returns:
/// - `0` on success (`*out_event` populated; pointer fields borrow)
/// - `TST_E_END_OF_STREAM` (-12) on graceful peer close
/// - `TST_E_CLOSED` (-7) if the handle was `_close`'d, or on any call AFTER
///   the first one that observed a cross-thread `_cancel` (that first call
///   reports `TST_E_TRANSPORT`; 0.7.0's WP-C2 makes it `TST_E_CLOSED` too)
/// - `TST_E_TRANSPORT` (-8) on transport failure
/// - `TST_E_INVALID_TS` (-3) on a demuxer error (strict-mode rejection
///   or unrecoverable packet malformation)
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments
///
/// On any non-zero return the contents of `*out_event` are unspecified.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_receiver_recv_event(
    p: *mut TstDemuxReceiver,
    out_event: *mut TstEvent,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    if out_event.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_event pointer");
        return TstError::InvalidConfig as i32;
    }
    // See `tst_receiver_recv_packet` for why the cancel latch is read on both
    // sides of the park and why `broken_is_eos` is `true` on a plain shell
    // (peer FIN surfaces as Broken from libsrt; `ManagedRecvTransport`
    // retries internally, so a Broken reaching a PLAIN receiver is a peer
    // close).
    let cancelled = handle.inner.is_cancelled();
    handle.inner.with_inner_mut(|rx| match rx.recv_event() {
        Ok(Some(ev)) => {
            let mut arena = handle.arena.lock().expect("event arena Mutex poisoned");
            // SAFETY: out_event non-null per guard above. event::convert
            // writes through the pointer; pointer fields on the result
            // alias the arena Vecs (held under the arena Mutex for the
            // duration of this call; the arena Mutex is released before
            // the closure returns, but Vec base pointers are stable
            // until the next convert() call which re-clears them — see
            // the design §4.5 lifetime contract).
            unsafe {
                crate::event::convert(&mut arena, &ev, &mut *out_event);
            }
            0
        }
        Ok(None) => record_recv_closed(cancelled || handle.inner.is_cancelled()),
        Err(e) => record_recv_error(&e, cancelled || handle.inner.is_cancelled(), true),
    })
}

/// Cancel a `tst_demux_receiver_t`. Unblocks a thread parked in
/// `_recv_event` within one libsrt I/O cycle (~3-10 ms) by closing
/// the underlying libsrt socket. Safe to call from any thread.
/// Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, `_recv_event` never reports `TST_E_END_OF_STREAM`, and the
/// rule is ORDINAL, not park-state: the FIRST call that observes the cancel
/// returns `TST_E_TRANSPORT` (-8) — libsrt reports the closed socket as a
/// broken connection, and `SrtTransport` nulls its socket slot on that error
/// — and EVERY LATER call returns `TST_E_CLOSED` (-7) off the now-empty
/// slot. `_cancel` itself never closes the shell. 0.7.0's WP-C2 makes the
/// first call report `TST_E_CLOSED` too. The handle must still be `_close`'d
/// to free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_receiver_cancel(p: *mut TstDemuxReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}
