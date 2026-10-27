//! Mount-handle stats + stream-handle getters.
//!
//! Five entry points:
//!
//! ```text
//! tst_rtsp_mount_get_stats(handle, *out)   — snapshot per-mount counters
//! tst_rtsp_mount_video_handle(handle)      — first video stream handle (u32)
//! tst_rtsp_mount_klv_handle(handle)        — first KLV stream handle (u32)
//! tst_rtsp_mount_audio_handle(handle)      — first audio stream handle (u32)
//! tst_rtsp_mount_subtitle_handle(handle)   — first subtitle stream handle (u32)
//! ```
//!
//! # Stream-handle conventions
//!
//! The `*_handle` getters return the FIRST configured stream of that kind.
//! For single-stream mounts (the typical case) this is always the right
//! handle. For multi-stream mounts, use the `push_*_to` variants combined
//! with the Rust-side `*_handles()` accessor — there are no multi-handle
//! C getters here (the `_by_index` variant is deferred until a consumer
//! asks for it).
//!
//! Return value: `TST_INVALID_STREAM_HANDLE` (`u32::MAX`) when no stream of
//! that kind is configured on the mount, or when the handle is NULL / the
//! inner muxer mutex is poisoned.
//!
//! # Error mapping
//!
//! - NULL `handle` or NULL `out` → `TST_E_INVALID_CONFIG` (-1), last-error set.
//! - Inner mutex poisoned → `TST_E_INTERNAL` (-10), last-error set.
//! - No stream of the requested kind → `TST_INVALID_STREAM_HANDLE` returned;
//!   no last-error is set (not an error condition — callers can test the
//!   return value).

use crate::error::{TstError, set_last_error};
use crate::handle::{
    TST_INVALID_STREAM_HANDLE, TstAudioStreamHandle, TstKlvStreamHandle, TstSubtitleStreamHandle,
    TstVideoStreamHandle,
};
use crate::panic::ffi_catch;
use crate::rtsp::server::types::TstRtspMountHandle;
use crate::stats::{TstMountStats, fill_mount_stats};

/// Snapshot per-mount stats into `*out`.
///
/// `out` must be a non-NULL pointer to a caller-allocated `tst_mount_stats_t`
/// struct. The struct is filled from the mount's internal stat counters
/// (bytes_pushed, packets_pushed, live peer_count, frames_dropped_total).
///
/// Returns 0 (`TST_E_SUCCESS`) on success, negative `TST_E_*` on failure.
///
/// # Safety
///
/// - `handle` must be a non-NULL, non-freed pointer from
///   `tst_rtsp_server_add_unicast_mount` / `tst_rtsp_server_add_multicast_mount`.
/// - `out` must be a non-NULL, writable pointer to a `tst_mount_stats_t`
///   struct valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_get_stats(
    handle: *const TstRtspMountHandle,
    out: *mut TstMountStats,
) -> libc::c_int {
    ffi_catch(TstError::Internal as libc::c_int, || {
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "out is null");
            return TstError::InvalidConfig as libc::c_int;
        }
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "handle is null");
            return TstError::InvalidConfig as libc::c_int;
        };
        let snapshot = h.inner.stats();
        // SAFETY: caller guarantees out is a valid, writable pointer.
        let dst = unsafe { &mut *out };
        fill_mount_stats(dst, &snapshot);
        TstError::Success as libc::c_int
    })
}

/// Return the first configured video stream handle for this mount.
///
/// Returns a `tst_video_stream_handle_t` (packed `u32`). Use the returned
/// value with `tst_rtsp_mount_push_video_to` to target a specific stream
/// on a multi-stream mount. For single-stream mounts this is equivalent to
/// using `tst_rtsp_mount_push_video` (which auto-selects the sole stream).
///
/// Returns `TST_INVALID_STREAM_HANDLE` (`UINT32_MAX`) when no video stream
/// is configured or when `handle` is NULL.
///
/// # Safety
///
/// - `handle` must be NULL, or a non-freed pointer from
///   `tst_rtsp_server_add_unicast_mount` / `tst_rtsp_server_add_multicast_mount`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_video_handle(
    handle: *const TstRtspMountHandle,
) -> TstVideoStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "handle is null");
            return TST_INVALID_STREAM_HANDLE;
        };
        match h.inner.video_handles().into_iter().next() {
            Some(vh) => vh.raw(),
            None => TST_INVALID_STREAM_HANDLE,
        }
    })
}

/// Return the first configured KLV stream handle for this mount.
///
/// Returns `TST_INVALID_STREAM_HANDLE` when no KLV stream is configured or
/// when `handle` is NULL. Use with `tst_rtsp_mount_push_klv_to`.
///
/// # Safety
///
/// - `handle` must be NULL, or a non-freed pointer from
///   `tst_rtsp_server_add_unicast_mount` / `tst_rtsp_server_add_multicast_mount`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_klv_handle(
    handle: *const TstRtspMountHandle,
) -> TstKlvStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "handle is null");
            return TST_INVALID_STREAM_HANDLE;
        };
        match h.inner.klv_handles().into_iter().next() {
            Some(kh) => kh.raw(),
            None => TST_INVALID_STREAM_HANDLE,
        }
    })
}

/// Return the first configured audio stream handle for this mount.
///
/// Returns `TST_INVALID_STREAM_HANDLE` when no audio stream is configured or
/// when `handle` is NULL. Use with `tst_rtsp_mount_push_audio_to`.
///
/// # Safety
///
/// - `handle` must be NULL, or a non-freed pointer from
///   `tst_rtsp_server_add_unicast_mount` / `tst_rtsp_server_add_multicast_mount`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_audio_handle(
    handle: *const TstRtspMountHandle,
) -> TstAudioStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "handle is null");
            return TST_INVALID_STREAM_HANDLE;
        };
        match h.inner.audio_handles().into_iter().next() {
            Some(ah) => ah.raw(),
            None => TST_INVALID_STREAM_HANDLE,
        }
    })
}

/// Return the first configured subtitle stream handle for this mount.
///
/// Returns `TST_INVALID_STREAM_HANDLE` when no subtitle stream is configured
/// or when `handle` is NULL. Use with `tst_rtsp_mount_push_subtitle_to`.
///
/// # Safety
///
/// - `handle` must be NULL, or a non-freed pointer from
///   `tst_rtsp_server_add_unicast_mount` / `tst_rtsp_server_add_multicast_mount`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_mount_subtitle_handle(
    handle: *const TstRtspMountHandle,
) -> TstSubtitleStreamHandle {
    ffi_catch(TST_INVALID_STREAM_HANDLE, || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "handle is null");
            return TST_INVALID_STREAM_HANDLE;
        };
        match h.inner.subtitle_handles().into_iter().next() {
            Some(sh) => sh.raw(),
            None => TST_INVALID_STREAM_HANDLE,
        }
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TstMuxConfig;
    use crate::config::streams::TstVideoCodec;
    use crate::handle::TstRtspServerBuilder;
    use crate::rtsp::server::types::TstRtspMountHandle;
    use crate::test_support::null_entry_tests;

    null_entry_tests! {
        null_handle_get_stats_returns_error => invalid_config {
            let mut out = TstMountStats::default();
            tst_rtsp_mount_get_stats(std::ptr::null(), &mut out)
        };
        null_handle_video_handle_returns_invalid => eq(TST_INVALID_STREAM_HANDLE) {
            tst_rtsp_mount_video_handle(std::ptr::null())
        };
        null_handle_klv_handle_returns_invalid => eq(TST_INVALID_STREAM_HANDLE) {
            tst_rtsp_mount_klv_handle(std::ptr::null())
        };
        null_handle_audio_handle_returns_invalid => eq(TST_INVALID_STREAM_HANDLE) {
            tst_rtsp_mount_audio_handle(std::ptr::null())
        };
        null_handle_subtitle_handle_returns_invalid => eq(TST_INVALID_STREAM_HANDLE) {
            tst_rtsp_mount_subtitle_handle(std::ptr::null())
        };
    }

    fn start_test_server() -> *mut crate::rtsp::server::types::TstRtspServer {
        let b = TstRtspServerBuilder::from_url("rtsp://127.0.0.1:0").expect("test url parses");
        let raw_b = TstRtspServerBuilder::into_raw(Box::new(b));
        unsafe { crate::rtsp::server::start::tst_rtsp_server_builder_start(raw_b) }
    }

    fn make_h264_mux_cfg() -> *mut TstMuxConfig {
        unsafe {
            let p = crate::config::tst_mux_config_new();
            let prog = crate::config::tst_mux_config_add_program(p, 1, 0x1000);
            let _v = crate::config::streams::tst_mux_config_add_video_stream(
                p,
                prog,
                0x1011,
                TstVideoCodec::H264 as i32,
            );
            p
        }
    }

    fn add_unicast_mount(
        server: *mut crate::rtsp::server::types::TstRtspServer,
        path: &str,
    ) -> *mut TstRtspMountHandle {
        let cfg = make_h264_mux_cfg();
        let cpath = std::ffi::CString::new(path).unwrap();
        let mount = unsafe {
            crate::rtsp::server::mount::tst_rtsp_server_add_unicast_mount(
                server,
                cpath.as_ptr(),
                cfg as *const _,
            )
        };
        unsafe { crate::config::tst_mux_config_free(cfg) };
        mount
    }

    #[test]
    fn null_out_get_stats_returns_error() {
        let server = start_test_server();
        assert!(!server.is_null());
        let mount = add_unicast_mount(server, "/live");
        assert!(!mount.is_null());
        let rc = unsafe { tst_rtsp_mount_get_stats(mount as *const _, std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as libc::c_int);
        unsafe { crate::rtsp::server::mount::tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn get_stats_on_fresh_mount_returns_zeros() {
        let server = start_test_server();
        assert!(!server.is_null());
        let mount = add_unicast_mount(server, "/live");
        assert!(!mount.is_null());
        let mut out = TstMountStats::default();
        let rc = unsafe { tst_rtsp_mount_get_stats(mount as *const _, &mut out) };
        assert_eq!(rc, TstError::Success as libc::c_int);
        assert_eq!(out.bytes_pushed, 0);
        assert_eq!(out.packets_pushed, 0);
        assert_eq!(out.peer_count, 0);
        assert_eq!(out.frames_dropped_total, 0);
        unsafe { crate::rtsp::server::mount::tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn video_handle_returns_valid_handle_for_h264_mount() {
        let server = start_test_server();
        assert!(!server.is_null());
        let mount = add_unicast_mount(server, "/live");
        assert!(!mount.is_null());
        let vh = unsafe { tst_rtsp_mount_video_handle(mount as *const _) };
        assert_ne!(
            vh, TST_INVALID_STREAM_HANDLE,
            "expected a valid video handle; got INVALID"
        );
        unsafe { crate::rtsp::server::mount::tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }

    #[test]
    fn klv_handle_returns_invalid_for_video_only_mount() {
        let server = start_test_server();
        assert!(!server.is_null());
        let mount = add_unicast_mount(server, "/live");
        assert!(!mount.is_null());
        let kh = unsafe { tst_rtsp_mount_klv_handle(mount as *const _) };
        assert_eq!(
            kh, TST_INVALID_STREAM_HANDLE,
            "expected INVALID for a video-only mount"
        );
        unsafe { crate::rtsp::server::mount::tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }
}
