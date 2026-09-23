//! `tst_managed_demux_receiver_t` — reconnect-aware sibling of the
//! plain `tst_demux_receiver_t`.
//!
//! Wraps a `ManagedDemuxReceiver<SrtTransport>`. Per-reconnect sync /
//! demux state reset is automatic — a `ReconnectDiscontinuity` event
//! surfaces from `_recv_event` after each transport reconnect
//! (validate-1 Sprint 4 F2 + followup-1). Open / lifecycle / event /
//! stats surfaces mirror the plain receiver one-for-one with `managed_`
//! infix on every C entry. The two notable shape changes vs the plain
//! sibling:
//!
//! * `_open*` family takes an optional `*const TstReconnectPolicy`.
//! * `_recv_event` does NOT apply the Broken→EOS mapping the plain
//!   receiver does (ManagedRecvTransport retries internally on Broken;
//!   a Broken reaching this function is the inner-cancel-mutex-poisoned
//!   path — a hard transport failure, `TST_E_TRANSPORT`). Reconnect
//!   **budget exhaustion** is a different path: `ManagedRecvTransport`
//!   latches its inner transport `Closed` when the policy gives up, and
//!   the receive-side shell maps `Closed` to `TST_E_END_OF_STREAM` —
//!   same as a clean peer close, because SRT cannot distinguish "peer
//!   hung up" from "peer never came back" at this layer. Call
//!   [`tst_managed_demux_receiver_end_reason`] to tell a clean teardown
//!   apart from a give-up-after-retries.
//!
//! # Threading rules and `?x-recvtimeout=<ms>`
//!
//! The `_cancel` / `_close` / `_recv_event` family has family-wide
//! threading rules (safe teardown ordering, what's lock-free-pollable vs
//! mutex-gated), and the `_open*` family accepts a `tst-c`-flavor SRT URL
//! extension for a socket-level per-recv deadline. Not module rustdoc —
//! cbindgen doesn't carry `//!` comments into `tstrans.h` — so the full
//! contract is written directly on [`tst_managed_demux_receiver_recv_event`]
//! (threading) and [`tst_managed_demux_receiver_open`] (`?x-recvtimeout=`),
//! with short cross-references on the other family members.

use crate::config::TstReconnectPolicy;
use crate::demux_config::TstDemuxConfig;
use crate::error::{
    TstError, record_binding_error, record_recv_closed, record_recv_error, set_last_error,
};
use crate::event::{EventArena, TstEvent};
use crate::handle::CHandle;
use crate::sender::mux_sender::{parse_c_srt_url, parse_c_srt_url_listener};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tst_pipeline::ManagedDemuxReceiver;
use tst_pipeline::binding::BindingError;
use tst_srt::SrtTransport;
use tst_srt::SrtUrl;

use crate::stream_end_reason::TstStreamEndReason;

pub struct TstManagedDemuxReceiver {
    /// Snapshot = the five lock-free observers (`ManagedHandles`) captured
    /// at open, BEFORE the receiver moved into the slot: cancel, end
    /// reason, successful rebuilds, factory attempts, reconnect-in-progress.
    ///
    /// Reading them takes NO lock on the slot (unlike `with_inner_ref` /
    /// `with_inner_mut`, which share ONE mutex with `_recv_event` — a
    /// blocked `_recv_event` holds it for its ENTIRE duration, including
    /// any internal reconnect retry loop, so a stats read gated behind it
    /// could never observe `reconnecting == true` while a reconnect is
    /// actually in progress). That is what makes
    /// `tst_managed_demux_receiver_get_reconnect_stats` and `_end_reason`
    /// safe to poll from a watchdog thread while another thread is parked
    /// in `_recv_event`. The end-reason handle is ALSO attached to the
    /// `CHandle` itself (`with_end_reason`) so `_end_reason` reads it
    /// through `CHandle::end_reason()` after the receiver is gone.
    inner: CHandle<ManagedDemuxReceiver<SrtTransport>, tst_pipeline::binding::ManagedHandles>,
    arena: Mutex<EventArena>,
    stream_stats_buf: Mutex<Vec<crate::stats::TstStreamStats>>,
}

/// Open a `tst_managed_demux_receiver_t` with default demux options.
/// URL-driven mode dispatch matches `tst_demux_receiver_open`.
/// `policy` is the reconnect policy; pass NULL for default.
///
/// # `?x-recvtimeout=<ms>`
///
/// `srt_url` accepts a `tst-c`-flavor SRT URL extension with no
/// libsrt-URL or ffmpeg precedent (ffmpeg's `?timeout=` is a rejected,
/// known-but-unsupported key here). `?x-recvtimeout=<ms>` sets
/// `SRTO_RCVTIMEO` — a per-recv deadline on the connected socket. On
/// expiry, `_recv_event` returns the retryable `TST_E_BUFFER_FULL` (the
/// transport is still alive; call `_recv_event` again). The setting
/// **survives reconnect**: the internal factory rebuilds every new socket
/// from the same parsed URL config, so the deadline reapplies to each
/// fresh connection automatically. It does **not** bound a listener-mode
/// open's `accept()` wait (see `tst_managed_demux_receiver_open_listener`)
/// — libsrt's `srt_accept` ignores `SRTO_RCVTIMEO`, so a listener-mode
/// open can still block indefinitely for a first connection regardless of
/// this key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_open(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        managed_open_inner(url, policy, None)
    })
}

/// Explicit listener-mode open for the managed demux receiver.
///
/// `srt_url` accepts the same `?x-recvtimeout=<ms>` URL key as
/// [`tst_managed_demux_receiver_open`] — see that function's doc for the
/// full contract. It does NOT bound this call's own `accept()` wait
/// (libsrt's `srt_accept` ignores `SRTO_RCVTIMEO`); once a peer connects,
/// it governs `_recv_event`'s per-recv deadline on the accepted socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_open_listener(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
) -> *mut TstManagedDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url_listener(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        managed_open_inner(url, policy, None)
    })
}

/// Open with a TstDemuxConfig override. URL-driven mode dispatch.
///
/// `srt_url` accepts the same `?x-recvtimeout=<ms>` URL key as
/// [`tst_managed_demux_receiver_open`] — see that function's doc for the
/// full contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_open_with_config(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
    cfg: *const TstDemuxConfig,
) -> *mut TstManagedDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        let opts = unsafe { cfg.as_ref().map(|c| c.build_options()) };
        managed_open_inner(url, policy, opts)
    })
}

/// Explicit listener-mode open with a TstDemuxConfig override.
///
/// `srt_url` accepts the same `?x-recvtimeout=<ms>` URL key as
/// [`tst_managed_demux_receiver_open`] — see that function's doc for the
/// full contract, and [`tst_managed_demux_receiver_open_listener`]'s doc
/// for why it doesn't bound this call's own `accept()` wait.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_open_listener_with_config(
    srt_url: *const libc::c_char,
    policy: *const TstReconnectPolicy,
    cfg: *const TstDemuxConfig,
) -> *mut TstManagedDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let policy = match unsafe { policy.as_ref() } {
            Some(p) => p.inner.clone(),
            None => tst_pipeline::ReconnectPolicy::default(),
        };
        let url = match unsafe { parse_c_srt_url_listener(srt_url) } {
            Ok(u) => u,
            Err(()) => return std::ptr::null_mut(),
        };
        let opts = unsafe { cfg.as_ref().map(|c| c.build_options()) };
        managed_open_inner(url, policy, opts)
    })
}

/// One managed open for all four entry points. The composition — initial
/// open dispatched on `url.mode`, the re-open factory that re-dials or
/// RE-ACCEPTS the same URL through the shared `FactoryCancel` slot, the
/// decorator, the shell, and the five observers taken before the move —
/// lives in tst-srt (Arc 2 WP-A3). The FIRST accept runs through that same
/// slot, but nothing can fire it before this call returns (DEBT-16,
/// deferred in Arc 2).
fn managed_open_inner(
    url: SrtUrl,
    policy: tst_pipeline::ReconnectPolicy,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> *mut TstManagedDemuxReceiver {
    // A3's signature takes the demuxer options by value; `DemuxerConfig`'s
    // `Default` is exactly what `ManagedDemuxReceiver::new` applied here.
    let opts = opts.unwrap_or_default();
    let (rx, handles) = match tst_srt::shells::managed_demux_receiver_from_url(&url, policy, opts) {
        Ok(t) => t,
        Err(e) => {
            record_binding_error(BindingError::from(e));
            return std::ptr::null_mut();
        }
    };
    let end_reason = handles.end_reason.clone();
    let cancel = Arc::clone(&handles.cancel);
    Box::into_raw(Box::new(TstManagedDemuxReceiver {
        inner: CHandle::new(rx, cancel, handles).with_end_reason(end_reason),
        arena: Mutex::new(EventArena::new()),
        stream_stats_buf: Mutex::new(Vec::new()),
    }))
}

/// Block until one typed `TstEvent` is ready.
///
/// **Asymmetry with plain receiver:** plain `tst_demux_receiver_recv_event`
/// maps `TransportError::Broken` on a non-cancelled handle to
/// `TST_E_END_OF_STREAM`. The managed version does NOT apply that mapping —
/// `ManagedRecvTransport` retries internally on Broken, so a Broken
/// reaching this function is the inner-cancel-mutex-poisoned path: a hard
/// transport failure (`TST_E_TRANSPORT`), not end-of-stream.
///
/// **Reconnect budget exhaustion is a different path, and it is NOT
/// `TST_E_TRANSPORT`:** when the configured reconnect policy gives up,
/// `ManagedRecvTransport` latches its inner transport `Closed` (the same
/// state a clean peer close leaves it in — SRT cannot tell "peer hung up"
/// from "peer never came back" at this layer), and this function returns
/// `TST_E_END_OF_STREAM`, exactly like a clean end of stream. Call
/// [`tst_managed_demux_receiver_end_reason`] afterward to distinguish a
/// clean teardown (`CLEAN_TEARDOWN`) from a give-up-after-retries
/// (`TRANSPORT_FAILED`) or an explicit `_cancel` (`CANCELLED`).
///
/// # Threading and blocking behavior
///
/// This call **blocks with no per-call timeout of its own**; see
/// [`tst_managed_demux_receiver_open`]'s `?x-recvtimeout=<ms>` doc for a
/// socket-level deadline. During a managed reconnect it stays blocked for
/// the whole backoff — recv-side reconnect is always the blocking
/// `ReconnectMode::Blocking` shape; `ReconnectMode::Background` is
/// warn-and-ignored on the receive path.
///
/// [`tst_managed_demux_receiver_cancel`] is lock-free, callable from any
/// thread, and idempotent — it closes the underlying socket, which
/// unblocks a thread parked here within about one libsrt I/O cycle
/// (roughly 3-10 ms), after which this call returns `TST_E_CLOSED`.
/// [`tst_managed_demux_receiver_close`] is **not** safe to call
/// concurrently with a thread blocked here — it acquires the data-path
/// mutex and then frees the allocation. The safe teardown sequence is:
/// `_cancel` from the control thread, wait for the reader thread to
/// return from this call, then `_close` from the thread that owns the
/// handle.
///
/// [`tst_managed_demux_receiver_end_reason`] and
/// `tst_managed_demux_receiver_get_reconnect_stats` are lock-free
/// side-channel reads — safe to poll from a watchdog thread concurrently
/// with a thread blocked here. The other stats getters (`_get_stats`,
/// `_get_socket_stats`, etc.) are NOT side-channel — they still take the
/// data-path mutex, so a blocked call here blocks them too.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_recv_event(
    p: *mut TstManagedDemuxReceiver,
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
    // `broken_is_eos = false` — the decorator already retried on Broken, so
    // a Broken reaching here is a hard transport failure (see the asymmetry
    // note above). The latch is read on both sides of the park.
    let cancelled = handle.inner.is_cancelled();
    handle.inner.with_inner_mut(|rx| match rx.recv_event() {
        Ok(Some(ev)) => {
            // Poison = a panic mid-write on another call; the arena is
            // rewritten before it is read, so recover (binding poison
            // policy: readers recover).
            let mut arena = handle.arena.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                crate::event::convert(&mut arena, &ev, &mut *out_event);
            }
            0
        }
        Ok(None) => record_recv_closed(cancelled || handle.inner.is_cancelled()),
        Err(e) => record_recv_error(&e, cancelled || handle.inner.is_cancelled(), false),
    })
}

/// Cancel a `tst_managed_demux_receiver_t`. Same shape as the plain
/// sibling — side-channel cancel, no Mutex acquisition. Safe from
/// any thread. Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// Closes the underlying socket, which unblocks a thread parked in
/// [`tst_managed_demux_receiver_recv_event`] within about one libsrt I/O
/// cycle (roughly 3-10 ms). After cancel, `_recv_event` returns
/// `TST_E_CLOSED` (not `TST_E_END_OF_STREAM`). The handle must still be
/// `_close`'d to free — but NOT while a thread may still be blocked in
/// `_recv_event`; see [`tst_managed_demux_receiver_close`]'s doc for the
/// safe teardown ordering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_cancel(
    p: *mut TstManagedDemuxReceiver,
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

/// Read the recorded reason this `tst_managed_demux_receiver_t` receive
/// session ended, if any.
///
/// Writes `TstStreamEndReason::None` (returns `0`) when the session
/// hasn't ended yet, or ended through a path this arc doesn't
/// instrument. A recorded reason is data, not a getter failure — this
/// only returns a nonzero code for a null-pointer argument.
///
/// **Last-error is untouched only for the "hasn't ended yet" sub-case**
/// (`RecvEndReasonHandle::get()` returns `None` — no reason was ever
/// recorded, so any pending failure from an earlier call is still
/// readable). The "recorded but this binding doesn't map that variant
/// yet" sub-case is different: `convert_recv_end_reason`'s `#[non_exhaustive]`
/// wildcard fallback still degrades to `None` for the *return value*,
/// but it runs through the same unconditional last-error reset every
/// other arm does (see that function's doc) — so last-error IS reset to
/// `TST_E_SUCCESS` there, not left untouched. This only bites if
/// `tst-pipeline` adds a `RecvEndReason` variant before this binding's
/// match is updated for it (mirrors `crate::rtp::end_reason::convert_end_reason`'s
/// wildcard, which has the same property).
///
/// Reuses the RTP-side `TstStreamEndReason` enum — its RTSP-shaped
/// variant names (`SessionExpired`, `KeepaliveFailed`, `ProtocolError`)
/// are never produced on this SRT recv path; only three of its six
/// non-`None` variants apply here:
/// - `tst_pipeline::RecvEndReason::EndOfStream` → `CleanTeardown`
/// - `tst_pipeline::RecvEndReason::ReconnectExhausted` → `TransportFailed`
///   (the reconnect decorator exhausted its policy budget — the peer
///   never came back)
/// - `tst_pipeline::RecvEndReason::Cancelled` → `Cancelled`
///
/// **Last-error side effect on every ACTUALLY-recorded reason:** unlike
/// the "hasn't ended" case above, once the session has ended this getter
/// unconditionally resets the thread-local last-error channel to
/// `TST_E_SUCCESS` with an EMPTY detail message — none of the three
/// `RecvEndReason` variants above carry a detail string (unlike the RTP
/// side's `KeepaliveFailed`/`TransportFailed`/`ProtocolError`), so
/// `tst_get_last_error_str()` always reads `""` once a reason has been
/// recorded, never a stale message left over from some earlier,
/// unrelated failure. Same contract as
/// `tst_rtp_demux_receiver_end_reason` — see that function's doc for the
/// full rationale and `docs/binding-authors.md`. Read any pending
/// failure from an earlier call BEFORE calling this getter, or it is
/// overwritten.
///
/// Side-channel: reads directly off the end-reason handle captured at
/// open time WITHOUT acquiring this handle's data-path Mutex — same
/// rationale as `tst_managed_demux_receiver_cancel` (a concurrent
/// `_recv_event` may be blocked holding it). This is what makes the
/// getter safe to poll from a watchdog thread while another thread
/// drives `_recv_event`. One consequence: this call never itself
/// returns `TST_E_CLOSED` — after `_close` the whole handle is freed,
/// and calling anything on it, including this getter, is a
/// use-after-free the caller must avoid.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstManagedDemuxReceiver` opened
/// via one of the `tst_managed_demux_receiver_open*` functions. `out`
/// must point to a writable `TstStreamEndReason`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_end_reason(
    p: *mut TstManagedDemuxReceiver,
    out: *mut TstStreamEndReason,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "null out pointer");
            return TstError::InvalidConfig as i32;
        }
        let reason = match handle.inner.end_reason() {
            Some(r) => convert_recv_end_reason(&r),
            None => TstStreamEndReason::None,
        };
        // SAFETY: out non-null per guard above.
        unsafe { *out = reason };
        0
    })
}

/// Convert a recorded [`tst_pipeline::RecvEndReason`] to its C
/// discriminant. Only called by `tst_managed_demux_receiver_end_reason`
/// when it already holds a `Some` from `RecvEndReasonHandle::get()` —
/// i.e. every arm here corresponds to an ACTUALLY-RECORDED reason, never
/// the "hasn't ended yet" case (that short-circuits to
/// `TstStreamEndReason::None` at the call site without reaching this
/// function — see the getter's doc for why that split matters to the
/// last-error contract).
///
/// Every arm therefore unconditionally resets the thread-local
/// last-error channel to `TstError::Success` with an empty detail
/// message — `RecvEndReason` carries no per-variant message data (unlike
/// `tst_rtp::StreamEndReason`), so there is nothing to forward.
///
/// `RecvEndReason` is `#[non_exhaustive]` on the `tst-pipeline` side; a
/// future variant this binding doesn't know how to map yet degrades to
/// `None` rather than panicking, mirroring
/// `crate::rtp::end_reason::convert_end_reason`'s wildcard fallback.
fn convert_recv_end_reason(r: &tst_pipeline::RecvEndReason) -> TstStreamEndReason {
    use tst_pipeline::RecvEndReason;
    let converted = match r {
        RecvEndReason::EndOfStream => TstStreamEndReason::CleanTeardown,
        RecvEndReason::ReconnectExhausted => TstStreamEndReason::TransportFailed,
        RecvEndReason::Cancelled => TstStreamEndReason::Cancelled,
        _ => TstStreamEndReason::None,
    };
    set_last_error(TstError::Success, "");
    converted
}

/// Close and free a `tst_managed_demux_receiver_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
///
/// **NOT safe to call concurrently with a thread blocked in
/// [`tst_managed_demux_receiver_recv_event`]**: this function acquires
/// the data-path mutex and then frees the allocation out from under it.
/// The safe teardown sequence is: [`tst_managed_demux_receiver_cancel`]
/// from the control thread, wait for the reader thread to return from
/// `_recv_event`, then call this function from the thread that owns the
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_close(p: *mut TstManagedDemuxReceiver) {
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_get_stats(
    p: *mut TstManagedDemuxReceiver,
    out: *mut crate::stats::TstDemuxReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::managed_demux_receiver_get_stats(&handle.inner, out) }
}

/// Snapshot reconnect telemetry for a `tst_managed_demux_receiver_t` —
/// recv-side sibling of `tst_managed_sender_get_reconnect_stats` /
/// `tst_managed_mux_sender_get_reconnect_stats` /
/// `tst_managed_raw_sender_get_reconnect_stats`. Reuses the same
/// `tst_managed_transport_stats_t` struct.
///
/// Field semantics differ from the send-side getter in two ways:
///
/// * `gap_len`, `gap_messages_dropped`, `gap_bytes_dropped` are always
///   `0`. Unlike the send side's `ManagedTransport`, the recv-side
///   `ManagedRecvTransport`/`ManagedDemuxReceiver` has no gap buffer —
///   there is nothing to queue while disconnected on the receive path
///   (a receiver only ever consumes bytes that already arrived; it
///   cannot buffer bytes the peer hasn't sent yet), so eviction
///   telemetry is structurally inapplicable.
/// * `reconnect_attempts` counts every factory invocation (successful or
///   not) since open, exactly like the send side's
///   `ManagedTransportStats::reconnect_attempts`; `reconnect_successes`
///   counts the rebuilds that were installed. Before 0.7.0 the recv side
///   reported successes in BOTH fields, so a failing reconnect loop read
///   as "never attempted".
///
/// **Lock-free side-channel read**, same shape as
/// `tst_managed_demux_receiver_end_reason`: `reconnecting`,
/// `reconnect_attempts` and `reconnect_successes` are read directly off
/// the `Arc<AtomicU64>` / `Arc<AtomicBool>` observers snapshotted at open
/// time (`tst_pipeline::binding::ManagedHandles`), WITHOUT acquiring this
/// handle's data-path Mutex. This is load-bearing, not a style choice: a
/// thread blocked in `_recv_event` holds that Mutex for the entire call,
/// including any internal reconnect retry loop — a getter gated behind
/// that same lock could only ever observe `reconnecting == true` once
/// nothing is actually reconnecting, defeating the point of exposing the
/// flag. Reading the snapshotted atomics directly is what makes this
/// getter safe to poll from a watchdog thread while another thread
/// drives `_recv_event`, including mid-outage.
///
/// Returns 0 on success, or `TST_E_INVALID_CONFIG` if either pointer is
/// null. Unlike most getters on this handle, this one **never** returns
/// `TST_E_CLOSED` — it doesn't consult the handle's data-path state at
/// all, so it keeps returning the last-observed values after `_close`
/// too (same caveat as `tst_managed_demux_receiver_end_reason`: calling
/// anything on a freed handle, including this getter, is a
/// use-after-free the caller must avoid).
///
/// # Safety
///
/// `p` must be a valid `*mut TstManagedDemuxReceiver` opened via one of
/// the `tst_managed_demux_receiver_open*` functions. `out` must point
/// to a writable `tst_managed_transport_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_get_reconnect_stats(
    p: *mut TstManagedDemuxReceiver,
    out: *mut crate::stats::TstManagedTransportStats,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "null out pointer");
            return TstError::InvalidConfig as i32;
        }
        let h = handle.inner.snapshot();
        let stats = crate::stats::TstManagedTransportStats {
            reconnect_attempts: h.attempts.load(Ordering::Relaxed),
            reconnect_successes: h.reconnects.load(Ordering::Acquire),
            reconnecting: h.reconnecting.load(Ordering::Acquire),
            ..Default::default()
        };
        // SAFETY: out non-null per guard above.
        unsafe { *out = stats };
        0
    })
}

/// Managed sibling of [`tst_demux_receiver_get_stream_codec_stats`](super::stats::tst_demux_receiver_get_stream_codec_stats).
/// Returns the same values — codec stats live on the inner `Demuxer`,
/// so they persist across reconnect. No `TST_E_NOT_AVAILABLE` routing.
///
/// # Errors
///
/// * `TST_E_INVALID_CONFIG` — `p` or `out` is null
/// * `TST_E_CLOSED` — handle was closed via `tst_managed_demux_receiver_close`
/// * `TST_E_NOT_FOUND` — `pid` has never been observed on this handle
/// * `TST_E_INTERNAL` — internal panic caught at the FFI boundary
///
/// # Safety
///
/// `p` must be a valid pointer obtained from
/// `tst_managed_demux_receiver_open`; `out` must be a writable
/// `tst_stream_codec_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_get_stream_codec_stats(
    p: *mut TstManagedDemuxReceiver,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::managed_demux_receiver_get_stream_codec_stats(
            &handle.inner,
            pid,
            out,
            &format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this demux receiver)"
            ),
        )
    }
}

/// Managed sibling of [`tst_demux_receiver_get_socket_stats`](super::stats::tst_demux_receiver_get_socket_stats). Returns
/// `TST_E_NOT_AVAILABLE` when the reconnect loop currently has no live
/// inner socket.
///
/// # Safety
///
/// Caller MUST ensure `p` is a valid `*mut TstManagedDemuxReceiver`
/// opened via `tst_managed_demux_receiver_open` and `out` points to a
/// writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_get_socket_stats(
    p: *mut TstManagedDemuxReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::managed_demux_receiver_get_socket_stats(
            &handle.inner,
            out,
            "managed demux receiver socket stats unavailable (transport reconnecting or closed)",
        )
    }
}

/// Managed sibling of [`tst_demux_receiver_get_stream_last_seen_micros`](super::stats::tst_demux_receiver_get_stream_last_seen_micros).
/// Same semantics — `*out_epoch_micros` is `0` for a pid never observed on
/// this handle, 0 on success otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_get_stream_last_seen_micros(
    p: *mut TstManagedDemuxReceiver,
    pid: u16,
    out_epoch_micros: *mut u64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::managed_demux_receiver_get_stream_last_seen_micros(
            &handle.inner,
            pid,
            out_epoch_micros,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_reset_stats(
    p: *mut TstManagedDemuxReceiver,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::managed_demux_receiver_reset_stats(
        &handle.inner,
        &handle.stream_stats_buf,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_managed_demux_receiver_get_stream_stats(
    p: *mut TstManagedDemuxReceiver,
    out_array: *mut *const crate::stats::TstStreamStats,
    out_count: *mut libc::size_t,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::managed_demux_receiver_get_stream_stats(
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
//
// Null-pointer-only coverage lives here (no live socket required). Real
// end-reason / reconnect-stats assertions against a live managed demux
// receiver live in `bindings/c/tests/url_open/demux_receiver.rs`, which
// has the real-SRT-socket rendezvous harness these entry points need.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_end_reason_returns_invalid_config() {
        let mut out = TstStreamEndReason::None;
        let rc = unsafe { tst_managed_demux_receiver_end_reason(std::ptr::null_mut(), &mut out) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_reconnect_stats_returns_invalid_config() {
        let mut out = crate::stats::TstManagedTransportStats::default();
        let rc = unsafe {
            tst_managed_demux_receiver_get_reconnect_stats(std::ptr::null_mut(), &mut out)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    /// `RecvEndReason` variants carry no message data, so every
    /// ACTUALLY-recorded reason must reset last-error to `(Success, "")`
    /// — pinned directly against the conversion helper, independent of
    /// live-socket setup. Mirrors `convert_end_reason`'s own unit tests
    /// in `crate::rtp::end_reason`.
    #[test]
    fn convert_recv_end_reason_sets_empty_last_error_detail_for_every_variant() {
        for (reason, expected) in [
            (
                tst_pipeline::RecvEndReason::EndOfStream,
                TstStreamEndReason::CleanTeardown,
            ),
            (
                tst_pipeline::RecvEndReason::ReconnectExhausted,
                TstStreamEndReason::TransportFailed,
            ),
            (
                tst_pipeline::RecvEndReason::Cancelled,
                TstStreamEndReason::Cancelled,
            ),
        ] {
            crate::error::clear_last_error_for_test();
            let converted = convert_recv_end_reason(&reason);
            // TstStreamEndReason has no Debug impl; compare discriminants.
            assert_eq!(converted as i32, expected as i32, "mapping for {reason:?}");
            assert_eq!(
                crate::error::test_last_error_code(),
                TstError::Success as i32
            );
            assert_eq!(crate::error::test_last_error_msg(), "");
        }
    }
}
