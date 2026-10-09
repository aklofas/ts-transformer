//! RTSP publisher role: publish mounts and the on-demand publisher queue.
//!
//! ```text
//! tst_rtsp_server_add_publish_mount(server, path)       -> tst_rtsp_publish_mount_t*
//! tst_rtsp_server_next_publisher(server, ms, &mount)    -> 0 / TST_E_BUFFER_FULL / TST_E_CLOSED
//! tst_rtsp_server_remove_mount(server, path)            -> 0 / TST_E_RTSP_MOUNT
//!
//! tst_rtsp_publish_mount_path / _peer_count / _generation / _get_stats / _publisher_info
//! tst_rtsp_publish_mount_into_demux_receiver(mount, cfg) -> tst_rtp_demux_receiver_t*  (once)
//! tst_rtsp_publish_mount_cancel(mount)
//! tst_rtsp_publish_mount_free(mount)                     (never closes the mount)
//! ```
//!
//! A publish mount accepts ANNOUNCE / RECORD from one publisher at a time
//! and re-serves PLAY readers from the same TS bytes. The application reads
//! the published stream through the mount's transport, taken once with
//! `tst_rtsp_publish_mount_into_demux_receiver`; that transport outlives
//! publisher churn and stays silent between publishers.
//!
//! # Error mapping
//!
//! `RtspServerError` goes through the shared binding kind table
//! (`crate::error::record_with_context`): `MountNotFound`,
//! `DuplicateMount` and `InvalidMountPath` are `TST_E_RTSP_MOUNT`, a second
//! transport take (`TransportTaken`) is `TST_E_CLOSED`. A stopped server is
//! `TST_E_CLOSED` on every entry point, including a `next_publisher` call
//! that `tst_rtsp_server_stop` woke.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use tst_core::RecvTransport;
use tst_pipeline::DemuxReceiver;

use crate::demux_config::TstDemuxConfig;
use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::rtp::demux_receiver::TstRtpDemuxReceiver;
use crate::rtsp::server::types::{
    TST_RTSP_PEER_ADDR_LEN, TstRtspClockAlignment, TstRtspPublishMount, TstRtspPublishMountStats,
    TstRtspPublishShape, TstRtspPublisherInfo, TstRtspServer,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Clone the live server out of its handle, releasing the handle's lock.
/// `None` (last-error set) on a NULL handle, a poisoned lock, or a stopped
/// server.
fn live_server(server: *mut TstRtspServer) -> Result<Arc<tst_rtp::RtspServer>, i32> {
    let Some(handle) = (unsafe { server.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "server is null");
        return Err(TstError::InvalidConfig as i32);
    };
    let guard = match handle.inner.lock() {
        Ok(g) => g,
        Err(_) => {
            set_last_error(TstError::Internal, "server mutex poisoned");
            return Err(TstError::Internal as i32);
        }
    };
    match guard.as_ref() {
        Some(s) => Ok(Arc::clone(s)),
        None => {
            set_last_error(TstError::Closed, "server is stopped or freed");
            Err(TstError::Closed as i32)
        }
    }
}

/// Record an error from a server call and return its code. `Shutdown` is
/// `TST_E_CLOSED`, the code every server entry point documents after
/// `tst_rtsp_server_stop`: a stop on another thread can land between
/// [`live_server`] and the call, and the shared kind table would otherwise
/// report it as `TST_E_RTSP_SERVER`.
fn record_server_error(e: tst_rtp::RtspServerError, ctx: &str) -> i32 {
    if matches!(e, tst_rtp::RtspServerError::Shutdown) {
        set_last_error(TstError::Closed, "server is stopped");
        return TstError::Closed as i32;
    }
    crate::error::record_with_context(e, ctx)
}

/// Borrow a NUL-terminated UTF-8 path argument.
fn path_arg<'a>(path: *const c_char) -> Result<&'a str, i32> {
    if path.is_null() {
        set_last_error(TstError::InvalidConfig, "path is null");
        return Err(TstError::InvalidConfig as i32);
    }
    // SAFETY: caller guarantees a NUL-terminated string valid for this call.
    match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => Ok(s),
        Err(_) => {
            set_last_error(TstError::InvalidConfig, "path is not valid UTF-8");
            Err(TstError::InvalidConfig as i32)
        }
    }
}

/// Box a `PublishMountHandle` as a C handle. NULL (with
/// `TST_E_INVALID_CONFIG` recorded) if the mount path cannot be a C
/// string; `validate_mount_path` refuses control bytes, so this is a
/// defensive edge, never a silent empty path.
fn into_c_mount(inner: tst_rtp::PublishMountHandle) -> *mut TstRtspPublishMount {
    match CString::new(inner.mount_path()) {
        Ok(path_c) => Box::into_raw(Box::new(TstRtspPublishMount { inner, path_c })),
        Err(_) => {
            set_last_error(
                TstError::InvalidConfig,
                "publish mount path contains a NUL byte",
            );
            std::ptr::null_mut()
        }
    }
}

/// Borrow a mount handle, recording `TST_E_INVALID_CONFIG` on NULL.
fn mount_ref<'a>(mount: *const TstRtspPublishMount) -> Result<&'a TstRtspPublishMount, i32> {
    // SAFETY: caller guarantees NULL or a live handle.
    match unsafe { mount.as_ref() } {
        Some(m) => Ok(m),
        None => {
            set_last_error(TstError::InvalidConfig, "publish mount is null");
            Err(TstError::InvalidConfig as i32)
        }
    }
}

/// Write `v` through a `uint64_t*` out-pointer.
fn write_u64(out: *mut u64, v: u64) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "out is null");
        return TstError::InvalidConfig as i32;
    }
    // SAFETY: caller guarantees a writable uint64_t.
    unsafe { *out = v };
    TstError::Success as i32
}

fn alignment_to_c(a: tst_rtp::ClockAlignment) -> TstRtspClockAlignment {
    match a {
        tst_rtp::ClockAlignment::NotApplicable => TstRtspClockAlignment::NotApplicable,
        tst_rtp::ClockAlignment::Pending => TstRtspClockAlignment::Pending,
        tst_rtp::ClockAlignment::Provisional => TstRtspClockAlignment::Provisional,
        tst_rtp::ClockAlignment::SenderReport => TstRtspClockAlignment::SenderReport,
        // `ClockAlignment` is non_exhaustive: a variant added later reaches
        // C as "nothing aligned yet" until the C enum gains its twin.
        _ => TstRtspClockAlignment::Pending,
    }
}

/// `(shape, klv)` of a Rust `PublishShape`.
fn shape_to_c(s: tst_rtp::PublishShape) -> (TstRtspPublishShape, bool) {
    match s {
        tst_rtp::PublishShape::Mp2t => (TstRtspPublishShape::Mp2t, false),
        tst_rtp::PublishShape::Elementary { klv } => (TstRtspPublishShape::Elementary, klv),
        // `PublishShape` is non_exhaustive: every shape added later is an
        // elementary-track shape (MP2T is the only passthrough one).
        _ => (TstRtspPublishShape::Elementary, false),
    }
}

/// Copy `s` into `dst` NUL-terminated, truncating to fit (snprintf-style).
fn copy_truncated(s: &str, dst: &mut [c_char; TST_RTSP_PEER_ADDR_LEN]) {
    let n = s.len().min(TST_RTSP_PEER_ADDR_LEN - 1);
    for (d, b) in dst.iter_mut().zip(&s.as_bytes()[..n]) {
        *d = *b as c_char;
    }
    dst[n..].fill(0);
}

// ---------------------------------------------------------------------------
// Server-side entry points
// ---------------------------------------------------------------------------

/// Register a publish mount at `path` on a started server.
///
/// The server then accepts ANNOUNCE / RECORD against `path` from one
/// publisher at a time; the mount re-serves PLAY readers from the published
/// TS bytes, and the application reads them through
/// [`tst_rtsp_publish_mount_into_demux_receiver`].
///
/// `path` must start with `/`, must not contain URL-reserved characters such
/// as `?` or `#`, and must not already be registered.
///
/// Returns a non-NULL handle (free it with [`tst_rtsp_publish_mount_free`])
/// or NULL with last-error set: `TST_E_INVALID_CONFIG` for a NULL argument,
/// `TST_E_RTSP_MOUNT` for an invalid or duplicate path, `TST_E_CLOSED`
/// after `tst_rtsp_server_stop`.
///
/// # Safety
///
/// - `server` must be NULL or a live pointer from
///   `tst_rtsp_server_builder_start`.
/// - `path` must be NULL or a NUL-terminated string valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_add_publish_mount(
    server: *mut TstRtspServer,
    path: *const c_char,
) -> *mut TstRtspPublishMount {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let s = match live_server(server) {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        let path = match path_arg(path) {
            Ok(p) => p,
            Err(_) => return std::ptr::null_mut(),
        };
        match s.add_publish_mount(path) {
            Ok(h) => into_c_mount(h),
            Err(e) => {
                record_server_error(e, "add_publish_mount failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// Wait up to `timeout_ms` for the next publish mount an ANNOUNCE created
/// on demand (see `tst_rtsp_server_builder_accept_unregistered_publishers`).
///
/// Returns:
/// - `0` with `*out` set to a new handle (free it with
///   [`tst_rtsp_publish_mount_free`]); the announcing publisher already
///   holds the mount.
/// - `TST_E_BUFFER_FULL` (-4, the retryable backpressure code) with
///   `*out = NULL` when no mount arrived in time.
///   This is always the result when on-demand publishers are off.
/// - `TST_E_CLOSED` with `*out = NULL` once the server is stopped, including
///   when `tst_rtsp_server_stop` runs on another thread while this call
///   waits: the stop wakes it. A call that `stop` wakes may also run the
///   server's final teardown before returning.
///
/// Mounts come out in the order their ANNOUNCEs created them, each to
/// exactly one caller. Concurrent callers are served one at a time, so a
/// call made while another waits can wait longer than its own timeout.
///
/// The returned handle can name a mount that
/// [`tst_rtsp_server_remove_mount`] already removed while it waited in the
/// queue; a demux receiver taken from it then reads `TST_E_END_OF_STREAM`
/// at once. Treat it as already expired.
///
/// The server's hard cancel (`tst_rtsp_cancel_handle_cancel`) does not wake
/// this call; `tst_rtsp_server_stop` does. Do not call
/// `tst_rtsp_server_free` while another thread is inside this call: stop
/// the server first, let the call return, then free.
///
/// # Safety
///
/// - `server` must be NULL or a live pointer from
///   `tst_rtsp_server_builder_start`.
/// - `out` must be NULL or a writable `tst_rtsp_publish_mount_t*` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_next_publisher(
    server: *mut TstRtspServer,
    timeout_ms: u64,
    out: *mut *mut TstRtspPublishMount,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "out is null");
            return TstError::InvalidConfig as libc::c_int;
        }
        // SAFETY: `out` is non-NULL and writable per the contract.
        unsafe { *out = std::ptr::null_mut() };
        // The server lock is released before the wait (see `live_server`),
        // so `tst_rtsp_server_stop` can take it and wake this call.
        let s = match live_server(server) {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        match s.next_publisher(Duration::from_millis(timeout_ms)) {
            Ok(Some(h)) => {
                // SAFETY: as above.
                unsafe { *out = into_c_mount(h) };
                if unsafe { (*out).is_null() } {
                    return TstError::InvalidConfig as libc::c_int;
                }
                TstError::Success as libc::c_int
            }
            Ok(None) => {
                set_last_error(
                    TstError::BufferFull,
                    "no publisher arrived before the timeout",
                );
                TstError::BufferFull as libc::c_int
            }
            Err(e) => record_server_error(e, "next_publisher failed"),
        }
    })
}

/// Remove the mount at `path`, of any kind, and free the path for reuse.
///
/// A publish mount is closed: its publisher is sent RTSP Notice 5402
/// ("Server-Initiated TEARDOWN") and disconnected, its PLAY readers end
/// with their sessions, and a demux receiver taken from it reads
/// `TST_E_END_OF_STREAM` once it has drained what was already queued. Its
/// `tst_rtsp_publish_mount_t` handles stay valid for the getters and
/// [`tst_rtsp_publish_mount_free`]. A unicast or multicast mount's handle
/// keeps accepting pushes, which reach nobody.
///
/// This is how an application removes idle on-demand mounts; freeing a
/// handle does not.
///
/// Blocks for the Notice writes (bounded at 1 s per session).
///
/// Returns `0`, `TST_E_RTSP_MOUNT` when no mount is registered at `path`,
/// `TST_E_INVALID_CONFIG` for a NULL argument, or `TST_E_CLOSED` after
/// `tst_rtsp_server_stop`.
///
/// # Safety
///
/// - `server` must be NULL or a live pointer from
///   `tst_rtsp_server_builder_start`.
/// - `path` must be NULL or a NUL-terminated string valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_remove_mount(
    server: *mut TstRtspServer,
    path: *const c_char,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        let s = match live_server(server) {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        let path = match path_arg(path) {
            Ok(p) => p,
            Err(rc) => return rc,
        };
        match s.remove_mount(path) {
            Ok(()) => TstError::Success as libc::c_int,
            Err(e) => record_server_error(e, "remove_mount failed"),
        }
    })
}

/// Read one of the server's publisher counters into `*out`.
fn server_counter(
    server: *mut TstRtspServer,
    out: *mut u64,
    pick: fn(&tst_rtp::ServerStats) -> u64,
) -> libc::c_int {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "out is null");
        return TstError::InvalidConfig as libc::c_int;
    }
    match live_server(server) {
        Ok(s) => write_u64(out, pick(&s.stats())),
        Err(rc) => rc,
    }
}

/// Publish mounts that currently have a publisher, into `*out`.
///
/// A counter beside `tst_server_stats_t` (whose layout does not change).
/// Returns `0`, `TST_E_INVALID_CONFIG` for a NULL argument, or
/// `TST_E_CLOSED` after `tst_rtsp_server_stop`.
///
/// # Safety
///
/// `server` must be NULL or a live server pointer; `out` must be NULL or a
/// writable `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_active_publishers(
    server: *mut TstRtspServer,
    out: *mut u64,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        server_counter(server, out, |s| s.active_publishers as u64)
    })
}

/// RTP packets received from publishers across every publish mount,
/// cumulative over the server's life, into `*out`.
///
/// Same return codes as [`tst_rtsp_server_active_publishers`].
///
/// # Safety
///
/// As [`tst_rtsp_server_active_publishers`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_total_rtp_packets_received(
    server: *mut TstRtspServer,
    out: *mut u64,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        server_counter(server, out, |s| s.total_rtp_packets_received)
    })
}

/// Bytes of the RTP packets counted by
/// [`tst_rtsp_server_total_rtp_packets_received`], headers included, into
/// `*out`.
///
/// Same return codes as [`tst_rtsp_server_active_publishers`].
///
/// # Safety
///
/// As [`tst_rtsp_server_active_publishers`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_total_rtp_bytes_received(
    server: *mut TstRtspServer,
    out: *mut u64,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        server_counter(server, out, |s| s.total_rtp_bytes_received)
    })
}

// ---------------------------------------------------------------------------
// Mount-side entry points
// ---------------------------------------------------------------------------

/// The mount path, NUL-terminated. Borrowed: valid until
/// [`tst_rtsp_publish_mount_free`] frees this handle. NULL (last-error
/// `TST_E_INVALID_CONFIG`) for a NULL handle.
///
/// # Safety
///
/// `mount` must be NULL or a live publish-mount handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_path(
    mount: *const TstRtspPublishMount,
) -> *const c_char {
    crate::panic::ffi_catch(std::ptr::null(), || match mount_ref(mount) {
        Ok(m) => m.path_c.as_ptr(),
        Err(_) => std::ptr::null(),
    })
}

/// Live PLAY readers on the mount, into `*out`. Works on a closed mount.
///
/// Returns `0` or `TST_E_INVALID_CONFIG` for a NULL argument.
///
/// # Safety
///
/// `mount` must be NULL or a live publish-mount handle; `out` must be NULL
/// or a writable `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_peer_count(
    mount: *const TstRtspPublishMount,
    out: *mut u64,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        match mount_ref(mount) {
            Ok(m) => write_u64(out, m.inner.peer_count() as u64),
            Err(rc) => rc,
        }
    })
}

/// Publishers that have ended on the mount, into `*out`. Works on a closed
/// mount.
///
/// Returns `0` or `TST_E_INVALID_CONFIG` for a NULL argument.
///
/// # Safety
///
/// As [`tst_rtsp_publish_mount_peer_count`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_generation(
    mount: *const TstRtspPublishMount,
    out: *mut u64,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        match mount_ref(mount) {
            Ok(m) => write_u64(out, m.inner.generation()),
            Err(rc) => rc,
        }
    })
}

/// Snapshot the mount's stats into `*out` (see
/// `tst_rtsp_publish_mount_stats_t`). Works on a closed mount.
///
/// Returns `0` or `TST_E_INVALID_CONFIG` for a NULL argument.
///
/// # Safety
///
/// `mount` must be NULL or a live publish-mount handle; `out` must be NULL
/// or a writable `tst_rtsp_publish_mount_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_get_stats(
    mount: *const TstRtspPublishMount,
    out: *mut TstRtspPublishMountStats,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        let m = match mount_ref(mount) {
            Ok(m) => m,
            Err(rc) => return rc,
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "out is null");
            return TstError::InvalidConfig as libc::c_int;
        }
        let s = m.inner.stats();
        let c = TstRtspPublishMountStats {
            rtp_packets_received: s.rtp_packets_received,
            bytes_received: s.bytes_received,
            malformed_packets: s.malformed_packets,
            source_rejected: s.source_rejected,
            frames_emitted: s.frames_emitted,
            frames_dropped_app: s.frames_dropped_app,
            frames_dropped_readers: s.frames_dropped_readers,
            aus_emitted: s.aus_emitted,
            aus_dropped: s.aus_dropped,
            aus_reordered: s.aus_reordered,
            klv_units_emitted: s.klv_units_emitted,
            klv_units_dropped: s.klv_units_dropped,
            alignment: alignment_to_c(s.alignment),
            alignment_steps: s.alignment_steps,
            ssrc_changes: s.ssrc_changes,
            generation: s.generation,
            peer_count: s.peer_count as u64,
        };
        // SAFETY: `out` is non-NULL and writable per the contract.
        unsafe { *out = c };
        TstError::Success as libc::c_int
    })
}

/// The publisher currently holding the mount, into `*out` (see
/// `tst_rtsp_publisher_info_t`). `out->present` is false, and every other
/// field zero, when no publisher holds it.
///
/// Returns `0` or `TST_E_INVALID_CONFIG` for a NULL argument.
///
/// # Safety
///
/// `mount` must be NULL or a live publish-mount handle; `out` must be NULL
/// or a writable `tst_rtsp_publisher_info_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_publisher_info(
    mount: *const TstRtspPublishMount,
    out: *mut TstRtspPublisherInfo,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        let m = match mount_ref(mount) {
            Ok(m) => m,
            Err(rc) => return rc,
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "out is null");
            return TstError::InvalidConfig as libc::c_int;
        }
        let mut c = TstRtspPublisherInfo::default();
        if let Some(p) = m.inner.publisher() {
            let (shape, klv) = shape_to_c(p.shape);
            c.present = true;
            c.shape = shape;
            c.klv = klv;
            c.generation = p.generation;
            c.since_unix_ms = p
                .since
                .duration_since(UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            copy_truncated(&p.peer.to_string(), &mut c.peer);
        }
        // SAFETY: `out` is non-NULL and writable per the contract.
        unsafe { *out = c };
        TstError::Success as libc::c_int
    })
}

/// End the application side of the mount: a demux receiver taken from it,
/// parked or not, reads `TST_E_CLOSED` (the same outcome as
/// `tst_rtp_demux_receiver_cancel`). Does not affect the publisher or
/// PLAY readers, and does not remove the mount. Idempotent.
///
/// Returns `0` or `TST_E_INVALID_CONFIG` for a NULL handle.
///
/// # Safety
///
/// `mount` must be NULL or a live publish-mount handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_cancel(
    mount: *mut TstRtspPublishMount,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as libc::c_int, || {
        match mount_ref(mount) {
            Ok(m) => {
                m.inner.cancel();
                TstError::Success as libc::c_int
            }
            Err(rc) => rc,
        }
    })
}

/// Take the mount's transport and return a `tst_rtp_demux_receiver_t` over
/// it, ready for `tst_rtp_demux_receiver_next_event`.
///
/// The transport is take-once across every handle to the mount: a second
/// call returns NULL with `TST_E_CLOSED`. The mount handle stays valid for
/// the getters and [`tst_rtsp_publish_mount_free`].
///
/// The receiver outlives publisher churn: between publishers it is open and
/// silent. It ends in one of two ways:
/// - `TST_E_CLOSED` after [`tst_rtsp_publish_mount_cancel`] or
///   `tst_rtp_demux_receiver_cancel` (an explicit cancel);
/// - `TST_E_END_OF_STREAM`, once what was already queued has drained, after
///   [`tst_rtsp_server_remove_mount`], `tst_rtsp_server_stop`, or
///   `tst_rtsp_server_free` (with no call in flight): the mount was closed.
///
/// A take on a mount already closed by `remove_mount` or `stop` (but never
/// taken) still succeeds, and the receiver reads `TST_E_END_OF_STREAM` at
/// once. The server's hard cancel (`tst_rtsp_cancel_handle_cancel`) does not
/// end it.
///
/// `demux_cfg` may be NULL (default options). Returns NULL with last-error
/// on failure; free the receiver with `tst_rtp_demux_receiver_close`.
///
/// # Safety
///
/// - `mount` must be NULL or a live publish-mount handle.
/// - `demux_cfg` must be NULL or a live pointer from
///   `tst_demux_config_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_into_demux_receiver(
    mount: *mut TstRtspPublishMount,
    demux_cfg: *const TstDemuxConfig,
) -> *mut TstRtpDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let m = match mount_ref(mount) {
            Ok(m) => m,
            Err(_) => return std::ptr::null_mut(),
        };
        let transport = match m.inner.clone().into_recv_transport() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "publish mount transport");
                return std::ptr::null_mut();
            }
        };
        // Both observers are captured before the transport moves into the
        // shell, exactly as `tst_rtsp_session_into_demux_receiver` does.
        let cancel = cancel_or_latch(transport.cancel_handle());
        let end_reason = transport.end_reason_handle();
        // SAFETY: caller guarantees NULL or a live demux config.
        let receiver = if let Some(cfg) = unsafe { demux_cfg.as_ref() } {
            DemuxReceiver::with_demux_options(transport, cfg.build_options())
        } else {
            DemuxReceiver::new(transport)
        };
        use crate::event::EventArena;
        use crate::rtp::RtpRecvSnap;
        Box::into_raw(Box::new(TstRtpDemuxReceiver {
            inner: CHandle::new(receiver, cancel, RtpRecvSnap { end_reason }),
            arena: Mutex::new(EventArena::new()),
            stream_stats_buf: Mutex::new(Vec::new()),
        }))
    })
}

/// Free a publish-mount handle. Never closes or removes the mount, which
/// lives in the server: use [`tst_rtsp_server_remove_mount`] for that. A
/// demux receiver taken from the mount keeps working. NULL is a no-op.
///
/// # Safety
///
/// `mount` must be NULL or a live publish-mount handle, not used again
/// after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_publish_mount_free(mount: *mut TstRtspPublishMount) {
    crate::panic::ffi_catch((), || {
        if mount.is_null() {
            return;
        }
        // SAFETY: caller guarantees a live, unaliased handle.
        let _ = unsafe { Box::from_raw(mount) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_truncated_terminates_and_truncates() {
        let mut buf = [0x7f as c_char; TST_RTSP_PEER_ADDR_LEN];
        copy_truncated("127.0.0.1:5000", &mut buf);
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert_eq!(s, "127.0.0.1:5000");
        let long = "x".repeat(100);
        copy_truncated(&long, &mut buf);
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert_eq!(s.len(), TST_RTSP_PEER_ADDR_LEN - 1);
    }

    #[test]
    fn server_shutdown_records_closed() {
        // A stop that lands between the handle read and the call surfaces
        // as `Shutdown`; every server entry point reports it as CLOSED.
        let rc = record_server_error(tst_rtp::RtspServerError::Shutdown, "remove_mount failed");
        assert_eq!(rc, TstError::Closed as i32);
        assert_eq!(
            crate::error::test_last_error_code(),
            TstError::Closed as i32
        );
        assert_eq!(crate::error::test_last_error_msg(), "server is stopped");
        // Every other error keeps the shared kind table's code.
        let rc = record_server_error(
            tst_rtp::RtspServerError::MountNotFound {
                path: "/x".to_owned(),
            },
            "remove_mount failed",
        );
        assert_eq!(rc, TstError::RtspMount as i32);
    }

    #[test]
    fn enum_mappings_cover_every_variant() {
        use tst_rtp::ClockAlignment as A;
        assert_eq!(
            alignment_to_c(A::NotApplicable),
            TstRtspClockAlignment::NotApplicable
        );
        assert_eq!(alignment_to_c(A::Pending), TstRtspClockAlignment::Pending);
        assert_eq!(
            alignment_to_c(A::Provisional),
            TstRtspClockAlignment::Provisional
        );
        assert_eq!(
            alignment_to_c(A::SenderReport),
            TstRtspClockAlignment::SenderReport
        );
        assert_eq!(
            shape_to_c(tst_rtp::PublishShape::Mp2t),
            (TstRtspPublishShape::Mp2t, false)
        );
        assert_eq!(
            shape_to_c(tst_rtp::PublishShape::Elementary { klv: true }),
            (TstRtspPublishShape::Elementary, true)
        );
    }
}
